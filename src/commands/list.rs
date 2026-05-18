//! `predict-cli list` — show every active expiry market by reading on-chain
//! state directly. v2 sources its market list from `PoolVault.active_expiry_markets`
//! instead of the predict-server `/oracles` endpoint, so the CLI stays
//! authoritative as soon as new markets are created.

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;

use crate::config::{is_v2_deploy_pending, FLOAT_SCALING, POOL_VAULT};
use crate::format::{fmt_countdown, fmt_expiry, label, shorten};
use crate::rpc::{pluck, u64_str, Rpc};

#[derive(serde::Serialize)]
struct ListedMarket {
    expiry_market_id: String,
    market_oracle_id: String,
    pyth_lazer_feed_id: u32,
    expiry: u64,
    allocated_capital: u64,
    lp_cash_balance: u64,
    is_compacted: bool,
}

pub async fn run(json: bool, all: bool) -> Result<()> {
    if is_v2_deploy_pending() {
        anyhow::bail!(
            "Predict v2 not deployed yet — POOL_VAULT is a placeholder. \
             See predict-cli/MIGRATION.md."
        );
    }

    let markets = enumerate_markets().await?;
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;

    // The `all` flag is reserved for parity with the v1 CLI; v2 considers
    // every active_expiry_markets entry "active." Settled/compacted markets
    // remain enumerable until the pool removes them.
    let filtered: Vec<&ListedMarket> = if all {
        markets.iter().collect()
    } else {
        markets
            .iter()
            .filter(|m| m.expiry > now_ms && !m.is_compacted)
            .collect()
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }

    println!(
        "{}  {} markets ({})",
        "DeepBook Predict — expiry markets (v2)".bold(),
        filtered.len(),
        if all { "all known" } else { "active" }
    );
    println!();
    println!(
        "  {:<8}  {:<22}  {:<14} {:<14} {:<5}  EXPIRY MARKET",
        "FEED", "EXPIRY", "ALLOCATED", "LP CASH", "STATE"
    );
    println!("  {}", "─".repeat(96).dimmed());
    for m in filtered {
        let allocated = (m.allocated_capital as f64) / 1_000_000.0;
        let cash = (m.lp_cash_balance as f64) / 1_000_000.0;
        let countdown = fmt_countdown(now_ms, m.expiry);
        let when = format!(
            "{}  {}",
            fmt_expiry(m.expiry),
            label(&format!("({})", countdown))
        );
        let state = if m.is_compacted {
            "comp".dimmed().to_string()
        } else if m.expiry <= now_ms {
            "pend".yellow().to_string()
        } else {
            "live".green().to_string()
        };
        println!(
            "  {:<8}  {:<22}  ${:<13.2} ${:<13.2} {:<5}  {}",
            m.pyth_lazer_feed_id,
            when,
            allocated,
            cash,
            state,
            shorten(&m.expiry_market_id),
        );
    }
    println!();
    println!(
        "  {}",
        "tip: copy any EXPIRY MARKET id into --oracle for mint/redeem/quote.".dimmed()
    );
    let _ = FLOAT_SCALING; // retained for parity with v1 binary; future tick reads use it
    Ok(())
}

async fn enumerate_markets() -> Result<Vec<ListedMarket>> {
    let rpc = Rpc::new();
    let vault = rpc.get_object(POOL_VAULT).await?;
    let vfields = pluck(&vault, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("pool vault: object content missing"))?;
    let ids = vfields
        .get("active_expiry_markets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("pool vault: missing active_expiry_markets vector"))?;

    let mut out = Vec::with_capacity(ids.len());
    for id_val in ids {
        let id = id_val
            .as_str()
            .ok_or_else(|| anyhow!("pool vault: non-string id in active_expiry_markets"))?;
        let market = rpc.get_object(id).await?;
        let mf = pluck(&market, &["data", "content", "fields"])
            .ok_or_else(|| anyhow!("expiry market {id}: content missing"))?;

        let market_oracle_id = mf
            .get("market_oracle_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let pyth_lazer_feed_id = mf
            .get("pyth_lazer_feed_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let expiry = u64_str(mf.get("expiry"));
        let allocated_capital = u64_str(mf.get("allocated_capital"));
        let lp_cash_balance = u64_str(mf.get("lp_cash_balance"));
        // is_compacted is the presence of `compacted_settlement: Option<u64>`
        // being Some. Serialized form is an object with `vec` of length 0 or 1.
        let is_compacted = mf
            .get("compacted_settlement")
            .and_then(|v| v.get("vec"))
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        out.push(ListedMarket {
            expiry_market_id: id.to_string(),
            market_oracle_id,
            pyth_lazer_feed_id,
            expiry,
            allocated_capital,
            lp_cash_balance,
            is_compacted,
        });
    }
    Ok(out)
}
