//! `predict-cli manager [--create]` — show or create the user's PredictManager.

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;

use crate::config::{PREDICT_PACKAGE, QUOTE_TYPE};
use crate::format::{fmt_usd, label};
use crate::rpc::{pluck, Rpc};
use crate::server;
use crate::sui_cli;

pub async fn run(create: bool, json: bool) -> Result<()> {
    sui_cli::check()?;
    let addr = sui_cli::active_address()?;
    let rpc = Rpc::new();

    if create {
        return run_create(&addr).await;
    }

    // Find the user's manager via PredictManagerCreated events on the server.
    let mgr = server::find_manager_for(&addr).await?;
    if let Some(m) = mgr {
        if json {
            // Also fetch on-chain balance.
            let obj = rpc.get_object(&m.manager_id).await.ok();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "manager": m,
                    "object": obj,
                }))?
            );
            return Ok(());
        }

        println!("{}", "PredictManager".bold());
        println!("  {} {}", label("owner"), addr);
        println!("  {} {}", label("manager id"), m.manager_id);
        println!("  {} {}", label("created at"), m.checkpoint_timestamp_ms);

        // Fetch DUSDC balance held inside the manager.
        if let Ok(obj) = rpc.get_object(&m.manager_id).await {
            if let Some(_owner) = pluck(&obj, &["data", "content", "fields", "owner"]) {
                let dusdc = rpc
                    .get_balance(&m.manager_id, Some(QUOTE_TYPE))
                    .await
                    .unwrap_or(0);
                println!(
                    "  {} {} (held inside manager BalanceManager)",
                    label("dusdc"),
                    fmt_usd((dusdc as f64) / 1_000_000.0)
                );
            }
        }
        // Wallet DUSDC balance.
        if let Ok(b) = rpc.get_balance(&addr, Some(QUOTE_TYPE)).await {
            println!(
                "  {} {} (in wallet)",
                label("dusdc wallet"),
                fmt_usd((b as f64) / 1_000_000.0)
            );
        }
        if let Ok(b) = rpc.get_balance(&addr, None).await {
            println!(
                "  {} {} SUI",
                label("sui wallet"),
                (b as f64) / 1_000_000_000.0
            );
        }
    } else {
        println!("{} {}", "no manager found for".dimmed(), addr);
        println!();
        println!("create one with: {}", "predict-cli manager --create".bold());
    }
    Ok(())
}

async fn run_create(_addr: &str) -> Result<()> {
    println!("Creating a PredictManager…");
    let args = vec![
        "--move-call".to_string(),
        format!("{PREDICT_PACKAGE}::predict::create_manager"),
    ];
    let out = sui_cli::run_ptb(args, 100_000_000)?;
    let digest =
        parse_digest(&out).ok_or_else(|| anyhow!("could not parse tx digest from sui output"))?;
    println!("  ✓ tx {}", digest);
    println!();
    println!(
        "  Wait a few seconds, then run `{}` to see the new manager.",
        "predict-cli manager".bold()
    );
    Ok(())
}

pub fn parse_digest(s: &str) -> Option<String> {
    // sui client ptb prints lines like `Transaction Digest: <DIGEST>`.
    for line in s.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Transaction Digest:") {
            return Some(rest.trim().to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("digest:") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}
