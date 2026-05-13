//! Trade commands: deposit, mint, mint-range, redeem, redeem-range, supply,
//! withdraw — every write path goes through `sui client ptb`.
//!
//! Important spend semantics: `predict::mint` reads from the manager's
//! aggregate balance. The CLI deposits `--deposit` USDC into the manager,
//! then submits the mint. To keep the default path spend-bounded, mint aborts
//! if the manager already has DUSDC unless `--allow-manager-balance` is passed.
//! `--max-cost` caps total manager funds available to the mint.

use anyhow::{anyhow, bail, Result};
use owo_colors::OwoColorize;

use crate::commands::manager::parse_digest;
use crate::config::{
    CLOCK_ID, FLOAT_SCALING, PREDICT_OBJECT, PREDICT_PACKAGE, QUOTE_DECIMALS, QUOTE_TYPE,
};
use crate::format::{fmt_usd, label, to_quote, to_scaled};
use crate::pricing::{binary_price, range_price, SviParams};
use crate::rpc::{option_u64, pluck, u64_str, Rpc};
use crate::server;
use crate::sui_cli;

const DEFAULT_GAS_BUDGET: u64 = 200_000_000;

/* ------------------------------------------------------------------ helpers */

fn validate_pos(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v <= 0.0 {
        bail!("{name} must be a positive finite number, got {v}");
    }
    Ok(())
}

fn validate_strike_range(lo: f64, hi: f64) -> Result<()> {
    validate_pos("--lower", lo)?;
    validate_pos("--upper", hi)?;
    if hi <= lo {
        bail!("--upper ({hi}) must be greater than --lower ({lo})");
    }
    Ok(())
}

async fn require_manager() -> Result<String> {
    let addr = sui_cli::active_address()?;
    let mgr = server::find_manager_for(&addr).await?.ok_or_else(|| {
        anyhow!(
            "no PredictManager found for {}. Create one: predict-cli manager --create",
            addr
        )
    })?;
    Ok(mgr.manager_id)
}

#[derive(Debug, Clone, Copy)]
struct OracleQuoteState {
    forward: f64,
    svi: SviParams,
    expiry: u64,
    active: bool,
    settlement: Option<f64>,
}

/// Return a DUSDC coin with enough balance. If funds are split, consolidate the
/// smallest needed set into the largest coin first.
async fn pick_funding_coin(amount_micro: u64) -> Result<String> {
    let addr = sui_cli::active_address()?;
    let rpc = Rpc::new();

    let selected = rpc
        .select_coins_for_amount(&addr, QUOTE_TYPE, amount_micro)
        .await?;
    let need_h = amount_micro as f64 / 10f64.powi(QUOTE_DECIMALS as i32);
    if selected.is_empty() {
        let total = rpc.get_balance(&addr, Some(QUOTE_TYPE)).await.unwrap_or(0);
        let total_h = total as f64 / 10f64.powi(QUOTE_DECIMALS as i32);
        bail!(
            "insufficient DUSDC. Need at least {:.6}; wallet balance is {:.6}.",
            need_h,
            total_h
        );
    }

    let primary = selected[0].id.clone();
    if selected.len() == 1 {
        return Ok(primary);
    }

    println!(
        "Consolidating {} DUSDC coins to fund {}…",
        selected.len(),
        fmt_usd(need_h)
    );
    for coin in selected.iter().skip(1) {
        let out = sui_cli::merge_coin(&primary, &coin.id)?;
        let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
        println!(
            "  ✓ merged {} into {} ({digest})",
            label(&coin.id),
            label(&primary)
        );
    }
    Ok(primary)
}

fn type_arg(t: &str) -> String {
    format!("<{t}>")
}

/// Read live oracle SviParams + forward + expiry, used for pre-flight quote.
async fn read_oracle_for_quote(oracle_id: &str) -> Result<OracleQuoteState> {
    let rpc = Rpc::new();
    let resp = rpc.get_object(oracle_id).await?;
    let fields = pluck(&resp, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("oracle: object content missing"))?;
    let active = fields
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let prices = pluck(fields, &["prices", "fields"])
        .cloned()
        .unwrap_or_default();
    let svi_node = pluck(fields, &["svi", "fields"])
        .cloned()
        .unwrap_or_default();
    let expiry = u64_str(fields.get("expiry"));
    let forward_scaled = u64_str(prices.get("forward"));
    let forward = forward_scaled as f64 / FLOAT_SCALING as f64;
    let settlement = fields
        .get("settlement_price")
        .and_then(option_u64)
        .map(|v| v as f64 / FLOAT_SCALING as f64);

    let svi = SviParams {
        a: u64_str(svi_node.get("a")) as f64 / FLOAT_SCALING as f64,
        b: u64_str(svi_node.get("b")) as f64 / FLOAT_SCALING as f64,
        rho: signed_i64(&svi_node, "rho"),
        m: signed_i64(&svi_node, "m"),
        sigma: u64_str(svi_node.get("sigma")) as f64 / FLOAT_SCALING as f64,
    };
    Ok(OracleQuoteState {
        forward,
        svi,
        expiry,
        active,
        settlement,
    })
}

fn signed_i64(svi_node: &serde_json::Value, key: &str) -> f64 {
    let Some(v) = svi_node.get(key) else {
        return 0.0;
    };
    let neg = pluck(v, &["fields", "is_negative"])
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let mag = pluck(v, &["fields", "magnitude"])
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<i128>().ok())
        .unwrap_or(0);
    let f = (mag as f64) / (FLOAT_SCALING as f64);
    if neg {
        -f
    } else {
        f
    }
}

/// Manager's current DUSDC balance held inside its embedded BalanceManager.
async fn manager_dusdc_balance(manager_id: &str) -> u128 {
    let rpc = Rpc::new();
    // DUSDC inside the manager is a Balance<DUSDC> inside the BalanceManager's
    // dynamic-field table, not a Coin. predict_manager_balance walks the table
    // and returns the real amount.
    rpc.predict_manager_balance(manager_id, QUOTE_TYPE)
        .await
        .unwrap_or(0) as u128
}

fn print_spend_preview(
    manager_existing_micro: u128,
    deposit_usdc: f64,
    fair_unit_price: f64,
    quantity: f64,
    max_cost: Option<f64>,
    allow_manager_balance: bool,
) -> Result<()> {
    let est_cost = fair_unit_price * quantity;
    let manager_existing = manager_existing_micro as f64 / 10f64.powi(QUOTE_DECIMALS as i32);
    let manager_after = manager_existing + deposit_usdc;

    println!();
    println!("  {}", "Spend preview".bold());
    println!(
        "    {} {} (in manager) + {} (this deposit) = {} available",
        label("balance"),
        fmt_usd(manager_existing),
        fmt_usd(deposit_usdc),
        fmt_usd(manager_after).bold()
    );
    let fair_cents = fair_unit_price * 100.0;
    println!(
        "    {} ~{:.0}¢ per unit  ·  qty {}  ·  estimated cost {}",
        label("fair value"),
        fair_cents,
        quantity,
        fmt_usd(est_cost).bold()
    );
    println!(
        "    {}",
        "the contract pulls from the manager balance at live mint price.".dimmed()
    );

    if manager_existing_micro > 0 && !allow_manager_balance {
        bail!(
            "manager already holds {}. To keep this mint's spend capped to the new deposit, \
             withdraw the existing balance or rerun with --allow-manager-balance.",
            fmt_usd(manager_existing)
        );
    }

    if let Some(max) = max_cost {
        if manager_after > max {
            bail!(
                "available manager funds after deposit ({}) exceed --max-cost {}. \
                 Reduce --deposit or withdraw manager funds before minting.",
                fmt_usd(manager_after),
                fmt_usd(max)
            );
        }
    }
    println!(
        "    {}",
        "hard cap: the mint cannot spend more than the available manager balance shown above."
            .dimmed()
    );
    Ok(())
}

fn assert_mintable_oracle(oracle_id: &str, state: OracleQuoteState) -> Result<()> {
    if let Some(settlement) = state.settlement {
        bail!(
            "oracle {oracle_id} is settled at {}; minting is closed",
            fmt_usd(settlement)
        );
    }
    if !state.active {
        bail!("oracle {oracle_id} is not active; minting is closed");
    }
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    if state.expiry <= now_ms {
        bail!("oracle {oracle_id} is expired and pending settlement; minting is closed");
    }
    if state.forward <= 0.0 {
        bail!("oracle {oracle_id} has no usable forward price");
    }
    Ok(())
}

/* -------------------------------------------------------------------- deposit */

pub async fn deposit(amount_usdc: f64) -> Result<()> {
    sui_cli::check()?;
    validate_pos("--amount", amount_usdc)?;
    let manager = require_manager().await?;
    let micro = to_quote("--amount", amount_usdc)?;
    let coin = pick_funding_coin(micro).await?;

    println!(
        "Depositing {} into manager {}…",
        fmt_usd(amount_usdc).bold(),
        label(&manager)
    );

    let args = vec![
        "--split-coins".into(),
        format!("@{coin}"),
        format!("[{micro}]"),
        "--assign".into(),
        "deposit_coin".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict_manager::deposit"),
        type_arg(QUOTE_TYPE),
        format!("@{manager}"),
        "deposit_coin.0".into(),
    ];
    let out = sui_cli::run_ptb(args, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ---------------------------------------------------------------- mint binary */

pub struct MintBinary {
    pub oracle_id: String,
    pub strike: f64,
    pub is_up: bool,
    pub quantity: f64,
    pub deposit: f64,
    pub max_cost: Option<f64>,
    pub allow_manager_balance: bool,
}

pub async fn mint_binary(args: MintBinary) -> Result<()> {
    sui_cli::check()?;
    validate_pos("--strike", args.strike)?;
    validate_pos("--qty", args.quantity)?;
    validate_pos("--deposit", args.deposit)?;
    if let Some(m) = args.max_cost {
        validate_pos("--max-cost", m)?;
    }

    let manager = require_manager().await?;
    let oracle = read_oracle_for_quote(&args.oracle_id).await?;
    assert_mintable_oracle(&args.oracle_id, oracle)?;
    let fair = binary_price(oracle.forward, args.strike, args.is_up, oracle.svi);

    let manager_existing = manager_dusdc_balance(&manager).await;
    print_spend_preview(
        manager_existing,
        args.deposit,
        fair,
        args.quantity,
        args.max_cost,
        args.allow_manager_balance,
    )?;

    let micro_deposit = to_quote("--deposit", args.deposit)?;
    let coin = pick_funding_coin(micro_deposit).await?;
    let strike_scaled = to_scaled("--strike", args.strike)?;
    // `quantity` on-chain is "quote tokens paid out on win" in DUSDC native
    // units (1e6), not 1e9. Earlier versions used to_scaled() and tripped
    // EBalanceManagerBalanceTooLow because every mint asked for 1000× more
    // DUSDC than the user expected.
    let qty_scaled = to_quote("--qty", args.quantity)?;
    let key_fn = if args.is_up { "up" } else { "down" };

    println!();
    println!(
        "Submitting {} {} ({} units)…",
        if args.is_up { "UP" } else { "DOWN" },
        format!("@${}", args.strike).bold(),
        args.quantity
    );

    let ptb = vec![
        "--split-coins".into(),
        format!("@{coin}"),
        format!("[{micro_deposit}]"),
        "--assign".into(),
        "deposit_coin".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict_manager::deposit"),
        type_arg(QUOTE_TYPE),
        format!("@{manager}"),
        "deposit_coin.0".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::market_key::{key_fn}"),
        format!("@{}", args.oracle_id),
        format!("{}u64", oracle.expiry),
        format!("{strike_scaled}u64"),
        "--assign".into(),
        "key".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::mint"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        format!("@{manager}"),
        format!("@{}", args.oracle_id),
        "key".into(),
        format!("{qty_scaled}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ----------------------------------------------------------------- mint range */

pub struct MintRange {
    pub oracle_id: String,
    pub lower: f64,
    pub upper: f64,
    pub quantity: f64,
    pub deposit: f64,
    pub max_cost: Option<f64>,
    pub allow_manager_balance: bool,
}

pub async fn mint_range(args: MintRange) -> Result<()> {
    sui_cli::check()?;
    validate_strike_range(args.lower, args.upper)?;
    validate_pos("--qty", args.quantity)?;
    validate_pos("--deposit", args.deposit)?;
    if let Some(m) = args.max_cost {
        validate_pos("--max-cost", m)?;
    }

    let manager = require_manager().await?;
    let oracle = read_oracle_for_quote(&args.oracle_id).await?;
    assert_mintable_oracle(&args.oracle_id, oracle)?;
    let lo_scaled = to_scaled("--lower", args.lower)?;
    let hi_scaled = to_scaled("--upper", args.upper)?;
    let fair = range_price(
        oracle.forward,
        lo_scaled,
        hi_scaled,
        FLOAT_SCALING as f64,
        oracle.svi,
    );

    let manager_existing = manager_dusdc_balance(&manager).await;
    print_spend_preview(
        manager_existing,
        args.deposit,
        fair,
        args.quantity,
        args.max_cost,
        args.allow_manager_balance,
    )?;

    let micro_deposit = to_quote("--deposit", args.deposit)?;
    let coin = pick_funding_coin(micro_deposit).await?;
    // `quantity` on-chain is "quote tokens paid out on win" in DUSDC native
    // units (1e6), not 1e9. Earlier versions used to_scaled() and tripped
    // EBalanceManagerBalanceTooLow because every mint asked for 1000× more
    // DUSDC than the user expected.
    let qty_scaled = to_quote("--qty", args.quantity)?;

    println!();
    println!(
        "Submitting BETWEEN ${}–${} ({} units)…",
        args.lower, args.upper, args.quantity
    );

    let ptb = vec![
        "--split-coins".into(),
        format!("@{coin}"),
        format!("[{micro_deposit}]"),
        "--assign".into(),
        "deposit_coin".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict_manager::deposit"),
        type_arg(QUOTE_TYPE),
        format!("@{manager}"),
        "deposit_coin.0".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::range_key::new"),
        format!("@{}", args.oracle_id),
        format!("{}u64", oracle.expiry),
        format!("{lo_scaled}u64"),
        format!("{hi_scaled}u64"),
        "--assign".into(),
        "rkey".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::mint_range"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        format!("@{manager}"),
        format!("@{}", args.oracle_id),
        "rkey".into(),
        format!("{qty_scaled}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ---------------------------------------------------------- redeem & redeem-range */

pub struct RedeemBinary {
    pub oracle_id: String,
    pub strike: f64,
    pub is_up: bool,
    pub quantity: f64,
    pub permissionless: bool,
}

pub async fn redeem_binary(args: RedeemBinary) -> Result<()> {
    sui_cli::check()?;
    validate_pos("--strike", args.strike)?;
    validate_pos("--qty", args.quantity)?;

    let manager = require_manager().await?;
    let strike_scaled = to_scaled("--strike", args.strike)?;
    // `quantity` on-chain is "quote tokens paid out on win" in DUSDC native
    // units (1e6), not 1e9. Earlier versions used to_scaled() and tripped
    // EBalanceManagerBalanceTooLow because every mint asked for 1000× more
    // DUSDC than the user expected.
    let qty_scaled = to_quote("--qty", args.quantity)?;
    let key_fn = if args.is_up { "up" } else { "down" };
    let predict_fn = if args.permissionless {
        "redeem_permissionless"
    } else {
        "redeem"
    };
    let expiry = read_oracle_for_quote(&args.oracle_id).await?.expiry;

    println!(
        "Redeeming {} {} ({} units, fn {})…",
        if args.is_up { "UP" } else { "DOWN" },
        format!("@${}", args.strike).bold(),
        args.quantity,
        predict_fn
    );

    let ptb = vec![
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::market_key::{key_fn}"),
        format!("@{}", args.oracle_id),
        format!("{expiry}u64"),
        format!("{strike_scaled}u64"),
        "--assign".into(),
        "key".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::{predict_fn}"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        format!("@{manager}"),
        format!("@{}", args.oracle_id),
        "key".into(),
        format!("{qty_scaled}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

pub struct RedeemRange {
    pub oracle_id: String,
    pub lower: f64,
    pub upper: f64,
    pub quantity: f64,
}

pub async fn redeem_range(args: RedeemRange) -> Result<()> {
    sui_cli::check()?;
    validate_strike_range(args.lower, args.upper)?;
    validate_pos("--qty", args.quantity)?;

    let manager = require_manager().await?;
    let lo_scaled = to_scaled("--lower", args.lower)?;
    let hi_scaled = to_scaled("--upper", args.upper)?;
    // `quantity` on-chain is "quote tokens paid out on win" in DUSDC native
    // units (1e6), not 1e9. Earlier versions used to_scaled() and tripped
    // EBalanceManagerBalanceTooLow because every mint asked for 1000× more
    // DUSDC than the user expected.
    let qty_scaled = to_quote("--qty", args.quantity)?;
    let expiry = read_oracle_for_quote(&args.oracle_id).await?.expiry;

    println!(
        "Redeeming BETWEEN ${}–${} ({} units)…",
        args.lower, args.upper, args.quantity
    );

    let ptb = vec![
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::range_key::new"),
        format!("@{}", args.oracle_id),
        format!("{expiry}u64"),
        format!("{lo_scaled}u64"),
        format!("{hi_scaled}u64"),
        "--assign".into(),
        "rkey".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::redeem_range"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        format!("@{manager}"),
        format!("@{}", args.oracle_id),
        "rkey".into(),
        format!("{qty_scaled}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ------------------------------------------------------------------ LP supply */

pub async fn supply(amount_usdc: f64) -> Result<()> {
    sui_cli::check()?;
    validate_pos("--amount", amount_usdc)?;
    let micro = to_quote("--amount", amount_usdc)?;
    let coin = pick_funding_coin(micro).await?;
    let addr = sui_cli::active_address()?;

    println!("Supplying {} to the LP vault…", fmt_usd(amount_usdc).bold());

    let ptb = vec![
        "--split-coins".into(),
        format!("@{coin}"),
        format!("[{micro}]"),
        "--assign".into(),
        "supply_coin".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::supply"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        "supply_coin.0".into(),
        format!("@{CLOCK_ID}"),
        "--assign".into(),
        "plp_coin".into(),
        "--transfer-objects".into(),
        "[plp_coin]".into(),
        format!("@{addr}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    println!("  PLP shares transferred to your address.");
    Ok(())
}

/* --------------------------------------------------------------- LP withdraw */

pub async fn withdraw(plp_coin_id: &str) -> Result<()> {
    sui_cli::check()?;
    if !plp_coin_id.starts_with("0x") {
        bail!("invalid PLP coin id: {plp_coin_id}");
    }
    let addr = sui_cli::active_address()?;
    println!("Withdrawing — burning PLP {}…", label(plp_coin_id));

    let ptb = vec![
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::predict::withdraw"),
        type_arg(QUOTE_TYPE),
        format!("@{PREDICT_OBJECT}"),
        format!("@{plp_coin_id}"),
        format!("@{CLOCK_ID}"),
        "--assign".into(),
        "out_coin".into(),
        "--transfer-objects".into(),
        "[out_coin]".into(),
        format!("@{addr}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_preview_blocks_existing_balance_by_default() {
        let err = print_spend_preview(1_000_000, 10.0, 0.5, 1.0, None, false)
            .expect_err("existing manager funds should require explicit opt-in");
        assert!(err.to_string().contains("already holds"));
    }

    #[test]
    fn spend_preview_enforces_hard_max_on_available_funds() {
        let err = print_spend_preview(5_000_000, 10.0, 0.5, 1.0, Some(12.0), true)
            .expect_err("manager funds after deposit exceed max");
        assert!(err.to_string().contains("exceed --max-cost"));
    }

    #[test]
    fn spend_preview_allows_deposit_within_cap() {
        print_spend_preview(0, 10.0, 0.5, 1.0, Some(10.0), false).unwrap();
    }

    #[test]
    fn mintable_oracle_rejects_closed_states() {
        let state = OracleQuoteState {
            forward: 80_000.0,
            svi: SviParams {
                a: 0.0,
                b: 0.0,
                rho: 0.0,
                m: 0.0,
                sigma: 0.0,
            },
            expiry: u64::MAX,
            active: false,
            settlement: None,
        };
        assert!(assert_mintable_oracle("0x1", state).is_err());

        let settled = OracleQuoteState {
            active: true,
            settlement: Some(80_000.0),
            ..state
        };
        assert!(assert_mintable_oracle("0x1", settled).is_err());
    }
}
