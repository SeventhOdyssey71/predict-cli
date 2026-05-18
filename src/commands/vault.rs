//! `predict-cli vault` — read the v2 `PoolVault` shared object.
//!
//! In v2 the vault is a single DUSDC-backed pool that fronts every active
//! ExpiryMarket. The aggregates it tracks are different from v1:
//!   - `idle_balance` — DUSDC sitting in the pool not yet allocated
//!   - `total_allocated_capital` — sum of expiry-market allocations
//!   - `total_supply` — outstanding PLP shares
//!   - `protocol_fee_balance` / `insurance_fee_balance` — accrued fees
//!   - `active_expiry_markets` — vector<ID> of in-flight expiry markets

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;

use crate::config::{is_v2_deploy_pending, POOL_VAULT, PROTOCOL_CONFIG, QUOTE_DECIMALS};
use crate::format::{fmt_usd, label};
use crate::rpc::{pluck, u64_str, Rpc};

pub async fn run(json: bool) -> Result<()> {
    if is_v2_deploy_pending() {
        anyhow::bail!(
            "Predict v2 not deployed yet — POOL_VAULT is a placeholder. \
             See predict-cli/MIGRATION.md."
        );
    }
    let rpc = Rpc::new();
    let resp = rpc.get_object(POOL_VAULT).await?;
    let fields = pluck(&resp, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("pool vault: object content missing"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(fields)?);
        return Ok(());
    }

    // idle_balance is `Balance<DUSDC>` which serializes as a u64 string under
    // the field name. fee balances are the same shape.
    let idle = u64_str(fields.get("idle_balance"));
    let allocated = u64_str(fields.get("total_allocated_capital"));
    let proto_fees = u64_str(fields.get("protocol_fee_balance"));
    let insur_fees = u64_str(fields.get("insurance_fee_balance"));

    let active = fields
        .get("active_expiry_markets")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    // Total PLP supply lives inside the TreasuryCap field — serialization
    // shape varies, so reach for `total_supply` if present.
    let plp_supply = pluck(
        fields,
        &["treasury_cap", "fields", "total_supply", "fields", "value"],
    )
    .and_then(|v| v.as_str())
    .and_then(|s| s.parse::<u64>().ok())
    .unwrap_or(0);

    let div = 10f64.powi(QUOTE_DECIMALS as i32);
    let idle_f = (idle as f64) / div;
    let allocated_f = (allocated as f64) / div;
    let proto_f = (proto_fees as f64) / div;
    let insur_f = (insur_fees as f64) / div;
    let total_capital_f = idle_f + allocated_f;
    let util_pct = if total_capital_f > 0.0 {
        (allocated_f / total_capital_f) * 100.0
    } else {
        0.0
    };

    println!("{}", "Predict PLP pool vault (v2)".bold());
    println!("  {} {}", label("pool vault"), POOL_VAULT);
    println!("  {} {}", label("protocol config"), PROTOCOL_CONFIG);
    println!();
    println!("  {} {}", label("idle balance"), fmt_usd(idle_f));
    println!("  {} {}", label("allocated capital"), fmt_usd(allocated_f));
    println!(
        "  {} {} ({:.2}%)",
        label("utilization"),
        fmt_usd(allocated_f),
        util_pct
    );
    println!();
    println!("  {} {}", label("active markets"), active);
    println!("  {} {} units", label("plp supply"), plp_supply);
    println!();
    println!("  {} {}", label("protocol fees"), fmt_usd(proto_f));
    println!("  {} {}", label("insurance fees"), fmt_usd(insur_f));
    Ok(())
}
