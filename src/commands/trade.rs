//! Trade commands (Predict v2 — DUSDC-backed parallel expiry markets).
//!
//! Every entry below targets the new module layout:
//!   - `expiry_market::mint` / `expiry_market::redeem` for trades
//!   - `expiry_market::range_key` for key construction (binary = range with ±∞)
//!   - `plp::supply` / `plp::withdraw` for LP flows (multi-step valuation PTB)
//!   - `predict_manager::deposit` / `withdraw` for DUSDC custody (no type-arg)
//!
//! User-facing CLI flags keep their v1 names (`--oracle`, `--strike`, etc.)
//! to minimize disruption — but `--oracle` now refers to the per-expiry
//! `ExpiryMarket` shared object ID. The CLI internally fetches the paired
//! `MarketOracle` and `PythSource` shared objects required by the entries.

use anyhow::{anyhow, bail, Result};
use owo_colors::OwoColorize;

use crate::commands::manager::parse_digest;
use crate::config::{
    is_v2_deploy_pending, CLOCK_ID, FLOAT_SCALING, NEG_INF, POOL_VAULT, POS_INF, PREDICT_PACKAGE,
    PROTOCOL_CONFIG, PYTH_SOURCE_BTC, PYTH_SOURCE_ETH, PYTH_SOURCE_SUI, QUOTE_DECIMALS, QUOTE_TYPE,
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

fn assert_v2_deployed() -> Result<()> {
    if is_v2_deploy_pending() {
        bail!(
            "Predict v2 not deployed yet — config.rs has unresolved TODO_V2 placeholders.\n\
             See predict-cli/MIGRATION.md for the deploy-watcher plan."
        );
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
struct MarketQuoteState {
    forward: f64,
    svi: SviParams,
    expiry: u64,
    is_settled: bool,
    settlement: Option<f64>,
}

/// Trio of shared object IDs needed by every trade PTB in v2.
#[derive(Debug, Clone)]
struct MarketHandles {
    /// `ExpiryMarket` shared object (the trade target).
    expiry_market_id: String,
    /// `MarketOracle` shared object paired with this expiry.
    market_oracle_id: String,
    /// `PythSource` shared object for the Pyth Lazer feed this market reads.
    pyth_source_id: String,
}

/// Resolve a user-supplied "oracle" arg (now the ExpiryMarket ID) into the
/// trio of shared objects the v2 Move calls require.
async fn fetch_market_handles(expiry_market_id: &str) -> Result<MarketHandles> {
    let rpc = Rpc::new();
    let market = rpc.get_object(expiry_market_id).await?;
    let fields = pluck(&market, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("expiry market {expiry_market_id}: object content missing"))?;
    let market_oracle_id = fields
        .get("market_oracle_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("expiry market {expiry_market_id}: missing market_oracle_id"))?
        .to_string();
    let pyth_feed_id = fields
        .get("pyth_lazer_feed_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("expiry market {expiry_market_id}: missing pyth_lazer_feed_id"))?;

    // Map Pyth Lazer feed ID -> shared PythSource object ID. Until the v2
    // deploy lands, these come from config.rs constants. After deploy, we
    // could alternatively call registry::pyth_source_id(feed_id) on-chain.
    let pyth_source_id = match pyth_feed_id {
        // BTC, ETH, SUI feed-id assignments are TBD — populated after deploy
        // when the admin runs registry::create_pyth_source for each asset.
        1 => PYTH_SOURCE_BTC,
        2 => PYTH_SOURCE_ETH,
        3 => PYTH_SOURCE_SUI,
        other => {
            bail!(
                "no PythSource configured for Pyth Lazer feed id {other}. \
                 Add it to config.rs."
            )
        }
    }
    .to_string();

    Ok(MarketHandles {
        expiry_market_id: expiry_market_id.to_string(),
        market_oracle_id,
        pyth_source_id,
    })
}

/// Return a DUSDC coin with enough balance.
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

/// Read pricing state for pre-flight quote display.
///
/// v2 splits live state across `MarketOracle` (SVI + spot/forward at update
/// time) and `PythSource` (real-time spot). For the spend preview we use the
/// MarketOracle's Block-Scholes-side numbers; live execution uses the fresh
/// oracle resolved on-chain.
async fn read_market_for_quote(expiry_market_id: &str) -> Result<MarketQuoteState> {
    let handles = fetch_market_handles(expiry_market_id).await?;
    let rpc = Rpc::new();
    let oracle = rpc.get_object(&handles.market_oracle_id).await?;
    let fields = pluck(&oracle, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("market oracle: object content missing"))?;

    let expiry = u64_str(fields.get("expiry"));
    let forward_scaled = u64_str(fields.get("block_scholes_forward"));
    let forward = forward_scaled as f64 / FLOAT_SCALING as f64;

    let svi_node = pluck(fields, &["block_scholes_svi", "fields"])
        .cloned()
        .unwrap_or_default();
    let svi = SviParams {
        a: u64_str(svi_node.get("a")) as f64 / FLOAT_SCALING as f64,
        b: u64_str(svi_node.get("b")) as f64 / FLOAT_SCALING as f64,
        rho: signed_i64(&svi_node, "rho"),
        m: signed_i64(&svi_node, "m"),
        sigma: u64_str(svi_node.get("sigma")) as f64 / FLOAT_SCALING as f64,
    };

    let settlement = fields
        .get("settlement_price")
        .and_then(option_u64)
        .map(|v| v as f64 / FLOAT_SCALING as f64);
    let is_settled = settlement.is_some();

    Ok(MarketQuoteState {
        forward,
        svi,
        expiry,
        is_settled,
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
/// The PredictManager is a shared object, not an address — `get_balance` on
/// the manager ID returns 0, so we walk the BalanceManager's dynamic fields
/// via the `predict_manager_balance` helper.
async fn manager_dusdc_balance(manager_id: &str) -> u128 {
    let rpc = Rpc::new();
    rpc.predict_manager_balance(manager_id, QUOTE_TYPE)
        .await
        .map(|v| v as u128)
        .unwrap_or(0)
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

fn assert_mintable_market(market_id: &str, state: MarketQuoteState) -> Result<()> {
    if let Some(settlement) = state.settlement {
        bail!(
            "market {market_id} is settled at {}; minting is closed",
            fmt_usd(settlement)
        );
    }
    if state.is_settled {
        bail!("market {market_id} is settled; minting is closed");
    }
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    if state.expiry <= now_ms {
        bail!("market {market_id} is expired and pending settlement; minting is closed");
    }
    if state.forward <= 0.0 {
        bail!("market {market_id} has no usable forward price");
    }
    Ok(())
}

/// Convenience: encode a price into the strike grid scaling.
fn strike_scaled(name: &str, value: f64) -> Result<u64> {
    to_scaled(name, value)
}

/* -------------------------------------------------------------------- deposit */

pub async fn deposit(amount_usdc: f64) -> Result<()> {
    sui_cli::check()?;
    assert_v2_deployed()?;
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
        // v2: no type-arg, DUSDC is implicit.
        format!("{PREDICT_PACKAGE}::predict_manager::deposit"),
        format!("@{manager}"),
        "deposit_coin.0".into(),
    ];
    let out = sui_cli::run_ptb(args, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

// Manager withdraw lives in `commands::manager::run_withdraw` in the
// standalone CLI; it's wired into the v2 surface there.

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

/// In v2 a binary UP/DOWN at `strike` is just a `RangeKey`:
///   UP   ⇒ `(strike, +∞)`
///   DOWN ⇒ `(-∞, strike)` ≡ `(0, strike)` because 0 is the neg-inf sentinel.
///
/// The Move entry is `expiry_market::mint`, shared with the range path —
/// only the key differs.
pub async fn mint_binary(args: MintBinary) -> Result<()> {
    sui_cli::check()?;
    assert_v2_deployed()?;
    validate_pos("--strike", args.strike)?;
    validate_pos("--qty", args.quantity)?;
    validate_pos("--deposit", args.deposit)?;
    if let Some(m) = args.max_cost {
        validate_pos("--max-cost", m)?;
    }

    let manager = require_manager().await?;
    let handles = fetch_market_handles(&args.oracle_id).await?;
    let market = read_market_for_quote(&args.oracle_id).await?;
    assert_mintable_market(&args.oracle_id, market)?;
    let fair = binary_price(market.forward, args.strike, args.is_up, market.svi);

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
    let strike_s = strike_scaled("--strike", args.strike)?;
    let qty_s = to_scaled("--qty", args.quantity)?;
    let (lower, higher) = if args.is_up {
        (strike_s, POS_INF)
    } else {
        (NEG_INF, strike_s)
    };

    println!();
    println!(
        "Submitting {} {} ({} units)…",
        if args.is_up {
            "UP".green().to_string()
        } else {
            "DOWN".red().to_string()
        },
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
        format!("@{manager}"),
        "deposit_coin.0".into(),
        "--move-call".into(),
        // v2: range_key is bound to a specific ExpiryMarket — only callable
        // through expiry_market::range_key (range_key::new is package-private).
        format!("{PREDICT_PACKAGE}::expiry_market::range_key"),
        format!("@{}", handles.expiry_market_id),
        format!("{lower}u64"),
        format!("{higher}u64"),
        "--assign".into(),
        "key".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::mint"),
        format!("@{}", handles.expiry_market_id),
        format!("@{PROTOCOL_CONFIG}"),
        format!("@{manager}"),
        format!("@{}", handles.market_oracle_id),
        format!("@{}", handles.pyth_source_id),
        "key".into(),
        format!("{qty_s}u64"),
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
    assert_v2_deployed()?;
    validate_strike_range(args.lower, args.upper)?;
    validate_pos("--qty", args.quantity)?;
    validate_pos("--deposit", args.deposit)?;
    if let Some(m) = args.max_cost {
        validate_pos("--max-cost", m)?;
    }

    let manager = require_manager().await?;
    let handles = fetch_market_handles(&args.oracle_id).await?;
    let market = read_market_for_quote(&args.oracle_id).await?;
    assert_mintable_market(&args.oracle_id, market)?;
    let lo_s = strike_scaled("--lower", args.lower)?;
    let hi_s = strike_scaled("--upper", args.upper)?;
    let fair = range_price(market.forward, lo_s, hi_s, FLOAT_SCALING as f64, market.svi);

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
    let qty_s = to_scaled("--qty", args.quantity)?;

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
        format!("@{manager}"),
        "deposit_coin.0".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::range_key"),
        format!("@{}", handles.expiry_market_id),
        format!("{lo_s}u64"),
        format!("{hi_s}u64"),
        "--assign".into(),
        "rkey".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::mint"),
        format!("@{}", handles.expiry_market_id),
        format!("@{PROTOCOL_CONFIG}"),
        format!("@{manager}"),
        format!("@{}", handles.market_oracle_id),
        format!("@{}", handles.pyth_source_id),
        "rkey".into(),
        format!("{qty_s}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ------------------------------------------------------- redeem binary/range */

pub struct RedeemBinary {
    pub oracle_id: String,
    pub strike: f64,
    pub is_up: bool,
    pub quantity: f64,
    /// v2: `expiry_market::redeem` handles live + settled + compacted in one
    /// entry. This flag is retained for CLI backwards-compat but ignored.
    pub permissionless: bool,
}

pub async fn redeem_binary(args: RedeemBinary) -> Result<()> {
    sui_cli::check()?;
    assert_v2_deployed()?;
    validate_pos("--strike", args.strike)?;
    validate_pos("--qty", args.quantity)?;
    let _ = args.permissionless;

    let manager = require_manager().await?;
    let handles = fetch_market_handles(&args.oracle_id).await?;
    let strike_s = strike_scaled("--strike", args.strike)?;
    let qty_s = to_scaled("--qty", args.quantity)?;
    let (lower, higher) = if args.is_up {
        (strike_s, POS_INF)
    } else {
        (NEG_INF, strike_s)
    };

    println!(
        "Redeeming {} {} ({} units)…",
        if args.is_up { "UP" } else { "DOWN" },
        format!("@${}", args.strike).bold(),
        args.quantity
    );

    let ptb = vec![
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::range_key"),
        format!("@{}", handles.expiry_market_id),
        format!("{lower}u64"),
        format!("{higher}u64"),
        "--assign".into(),
        "key".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::redeem"),
        format!("@{}", handles.expiry_market_id),
        format!("@{PROTOCOL_CONFIG}"),
        format!("@{manager}"),
        format!("@{}", handles.market_oracle_id),
        format!("@{}", handles.pyth_source_id),
        "key".into(),
        format!("{qty_s}u64"),
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
    assert_v2_deployed()?;
    validate_strike_range(args.lower, args.upper)?;
    validate_pos("--qty", args.quantity)?;

    let manager = require_manager().await?;
    let handles = fetch_market_handles(&args.oracle_id).await?;
    let lo_s = strike_scaled("--lower", args.lower)?;
    let hi_s = strike_scaled("--upper", args.upper)?;
    let qty_s = to_scaled("--qty", args.quantity)?;

    println!(
        "Redeeming BETWEEN ${}–${} ({} units)…",
        args.lower, args.upper, args.quantity
    );

    let ptb = vec![
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::range_key"),
        format!("@{}", handles.expiry_market_id),
        format!("{lo_s}u64"),
        format!("{hi_s}u64"),
        "--assign".into(),
        "rkey".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::expiry_market::redeem"),
        format!("@{}", handles.expiry_market_id),
        format!("@{PROTOCOL_CONFIG}"),
        format!("@{manager}"),
        format!("@{}", handles.market_oracle_id),
        format!("@{}", handles.pyth_source_id),
        "rkey".into(),
        format!("{qty_s}u64"),
        format!("@{CLOCK_ID}"),
    ];
    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    Ok(())
}

/* ------------------------------------------------------------------ LP flows */

/// Read `PoolVault.active_expiry_markets` (vector<ID>) and resolve each
/// market's paired MarketOracle + PythSource. This drives the valuation
/// prelude that `plp::supply` / `plp::withdraw` require.
async fn enumerate_active_markets() -> Result<Vec<MarketHandles>> {
    let rpc = Rpc::new();
    let vault = rpc.get_object(POOL_VAULT).await?;
    let fields = pluck(&vault, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("pool vault: object content missing"))?;
    let ids = fields
        .get("active_expiry_markets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("pool vault: missing active_expiry_markets vector"))?;

    let mut handles = Vec::with_capacity(ids.len());
    for id_val in ids {
        let id = id_val
            .as_str()
            .ok_or_else(|| anyhow!("pool vault: active_expiry_markets contained non-string"))?;
        handles.push(fetch_market_handles(id).await?);
    }
    Ok(handles)
}

/// Build the valuation-prelude PTB fragment.
///
/// Output is a `Vec<String>` that emits, in order:
///   start_valuation(vault, config) -> v
///   for each active market i:
///     read_valuation(market_i, config, oracle_i, pyth_i, clock) -> ev_i
///     add_expiry_valuation(v, ev_i)
fn valuation_prelude(handles: &[MarketHandles]) -> Vec<String> {
    let mut out = Vec::new();
    out.push("--move-call".into());
    out.push(format!("{PREDICT_PACKAGE}::plp::start_valuation"));
    out.push(format!("@{POOL_VAULT}"));
    out.push(format!("@{PROTOCOL_CONFIG}"));
    out.push("--assign".into());
    out.push("valuation".into());

    for (i, h) in handles.iter().enumerate() {
        let ev = format!("ev_{i}");
        out.push("--move-call".into());
        out.push(format!("{PREDICT_PACKAGE}::expiry_market::read_valuation"));
        out.push(format!("@{}", h.expiry_market_id));
        out.push(format!("@{PROTOCOL_CONFIG}"));
        out.push(format!("@{}", h.market_oracle_id));
        out.push(format!("@{}", h.pyth_source_id));
        out.push(format!("@{CLOCK_ID}"));
        out.push("--assign".into());
        out.push(ev.clone());

        out.push("--move-call".into());
        out.push(format!("{PREDICT_PACKAGE}::plp::add_expiry_valuation"));
        out.push("valuation".into());
        out.push(ev);
    }
    out
}

pub async fn supply(amount_usdc: f64) -> Result<()> {
    sui_cli::check()?;
    assert_v2_deployed()?;
    validate_pos("--amount", amount_usdc)?;
    let micro = to_quote("--amount", amount_usdc)?;
    let coin = pick_funding_coin(micro).await?;
    let addr = sui_cli::active_address()?;

    println!("Supplying {} to the PLP pool…", fmt_usd(amount_usdc).bold());
    let markets = enumerate_active_markets().await?;
    println!(
        "  Valuating across {} active expiry markets…",
        markets.len()
    );

    let mut ptb = valuation_prelude(&markets);
    ptb.extend([
        "--split-coins".into(),
        format!("@{coin}"),
        format!("[{micro}]"),
        "--assign".into(),
        "supply_coin".into(),
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::plp::supply"),
        format!("@{POOL_VAULT}"),
        format!("@{PROTOCOL_CONFIG}"),
        "valuation".into(),
        "supply_coin.0".into(),
        "--assign".into(),
        "plp_coin".into(),
        "--transfer-objects".into(),
        "[plp_coin]".into(),
        format!("@{addr}"),
    ]);

    let out = sui_cli::run_ptb(ptb, DEFAULT_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
    println!("  PLP shares transferred to your address.");
    Ok(())
}

pub async fn withdraw(plp_coin_id: &str) -> Result<()> {
    sui_cli::check()?;
    assert_v2_deployed()?;
    if !plp_coin_id.starts_with("0x") {
        bail!("invalid PLP coin id: {plp_coin_id}");
    }
    let addr = sui_cli::active_address()?;
    println!("Withdrawing — burning PLP {}…", label(plp_coin_id));

    let markets = enumerate_active_markets().await?;
    println!(
        "  Valuating across {} active expiry markets…",
        markets.len()
    );

    let mut ptb = valuation_prelude(&markets);
    ptb.extend([
        "--move-call".into(),
        format!("{PREDICT_PACKAGE}::plp::withdraw"),
        format!("@{POOL_VAULT}"),
        format!("@{PROTOCOL_CONFIG}"),
        "valuation".into(),
        format!("@{plp_coin_id}"),
        "--assign".into(),
        "out_coin".into(),
        "--transfer-objects".into(),
        "[out_coin]".into(),
        format!("@{addr}"),
    ]);

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
    fn mintable_market_rejects_closed_states() {
        let state = MarketQuoteState {
            forward: 80_000.0,
            svi: SviParams {
                a: 0.0,
                b: 0.0,
                rho: 0.0,
                m: 0.0,
                sigma: 0.0,
            },
            expiry: u64::MAX,
            is_settled: false,
            settlement: None,
        };
        let settled = MarketQuoteState {
            is_settled: true,
            settlement: Some(80_000.0),
            ..state
        };
        assert!(assert_mintable_market("0x1", settled).is_err());
    }

    #[test]
    fn binary_key_bounds_match_sentinels() {
        // UP @ strike means range (strike, +∞); DOWN means (-∞, strike).
        // Sanity-check the sentinel constants line up with Move's constants.
        assert_eq!(POS_INF, u64::MAX);
        assert_eq!(NEG_INF, 0);
    }
}
