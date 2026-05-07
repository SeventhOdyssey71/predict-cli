//! `predict-cli list` — show all active oracles via the predict-server.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::FLOAT_SCALING;
use crate::format::{fmt_countdown, fmt_expiry, fmt_strike, label, shorten};
use crate::server;

pub async fn run(json: bool, all: bool) -> Result<()> {
    let mut oracles = server::list_oracles().await?;
    if !all {
        oracles.retain(|o| o.status == "active");
    }
    oracles.sort_by_key(|o| o.expiry);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::to_value(&oracles)?)?
        );
        return Ok(());
    }

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    println!(
        "{}  {} oracles ({})",
        "DeepBook Predict — markets".bold(),
        oracles.len(),
        if all { "all" } else { "active" }
    );
    println!();
    println!(
        "  {:<12} {:<8}  {:<22}  {:<14} {:<10}  ORACLE",
        "ASSET", "STATUS", "EXPIRY", "MIN STRIKE", "TICK"
    );
    println!("  {}", "─".repeat(94).dimmed());
    for o in &oracles {
        let min = (o.min_strike as f64) / (FLOAT_SCALING as f64);
        let tick = (o.tick_size as f64) / (FLOAT_SCALING as f64);
        let countdown = fmt_countdown(now_ms, o.expiry);
        let when = format!(
            "{}  {}",
            fmt_expiry(o.expiry),
            label(&format!("({})", countdown))
        );
        let status_lbl = match o.status.as_str() {
            "active" => o.status.green().to_string(),
            "settled" => o.status.dimmed().to_string(),
            other => other.to_string(),
        };
        println!(
            "  {:<12} {:<8}  {:<22}  {:<14} {:<10}  {}",
            o.underlying_asset,
            status_lbl,
            when,
            fmt_strike(min, &o.underlying_asset),
            fmt_strike(tick, &o.underlying_asset),
            shorten(&o.oracle_id),
        );
    }
    Ok(())
}
