//! `predict-cli manager [--create] [--withdraw <amount>]` — show, create, or
//! withdraw from the user's PredictManager.

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;

use crate::config::{PREDICT_PACKAGE, QUOTE_TYPE};
use crate::format::{fmt_usd, label, to_quote};
use crate::rpc::Rpc;
use crate::server;
use crate::sui_cli;

const WITHDRAW_GAS_BUDGET: u64 = 100_000_000;

#[derive(Debug, Clone)]
pub struct Args {
    pub create: bool,
    pub withdraw: Option<f64>,
    pub json: bool,
}

pub async fn dispatch(args: Args) -> Result<()> {
    if args.create && args.withdraw.is_some() {
        anyhow::bail!("pass either --create or --withdraw, not both");
    }
    if let Some(amount) = args.withdraw {
        return run_withdraw(amount).await;
    }
    run(args.create, args.json).await
}

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

        // DUSDC inside the manager is held as a Balance<DUSDC> inside the
        // BalanceManager's dynamic-field table, not as a Coin object. The
        // suix_getBalance RPC only counts Coins, so it always reports 0 even
        // when the manager has funds. predict_manager_balance walks the
        // dynamic-field table directly and returns the actual amount.
        let dusdc_in_mgr = rpc
            .predict_manager_balance(&m.manager_id, QUOTE_TYPE)
            .await
            .unwrap_or(0);
        println!(
            "  {} {} (held inside manager BalanceManager)",
            label("dusdc"),
            fmt_usd((dusdc_in_mgr as f64) / 1_000_000.0)
        );
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
    use crate::config::PREDICT_REGISTRY;

    println!("Creating a PredictManager…");
    // v2: create_and_share_manager is an entry fn in `registry` that creates
    // the PredictManager (a derived_object keyed by sender) and shares it
    // in one call. No type-arg — DUSDC is implicit.
    let args = vec![
        "--move-call".to_string(),
        format!("{PREDICT_PACKAGE}::registry::create_and_share_manager"),
        format!("@{PREDICT_REGISTRY}"),
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

async fn run_withdraw(amount_usdc: f64) -> Result<()> {
    sui_cli::check()?;
    let addr = sui_cli::active_address()?;
    let rpc = Rpc::new();

    let mgr = server::find_manager_for(&addr).await?.ok_or_else(|| {
        anyhow!("no manager found for {addr}. Run `predict-cli manager --create` first.")
    })?;

    let available = rpc
        .predict_manager_balance(&mgr.manager_id, QUOTE_TYPE)
        .await
        .unwrap_or(0);
    let micro_amount = to_quote("--withdraw", amount_usdc)?;
    if micro_amount > available {
        anyhow::bail!(
            "manager only holds {} DUSDC; cannot withdraw ${amount_usdc:.4}",
            fmt_usd((available as f64) / 1_000_000.0)
        );
    }

    println!(
        "Withdrawing {} from manager → wallet…",
        fmt_usd(amount_usdc).bold()
    );

    // v2: predict_manager::withdraw returns Coin<DUSDC>; no type-arg.
    // Transfer it to the sender so it lands in the wallet.
    let ptb = vec![
        "--move-call".to_string(),
        format!("{PREDICT_PACKAGE}::predict_manager::withdraw"),
        format!("@{}", mgr.manager_id),
        format!("{micro_amount}u64"),
        "--assign".to_string(),
        "coin".to_string(),
        "--transfer-objects".to_string(),
        "[coin]".to_string(),
        format!("@{addr}"),
    ];
    let out = sui_cli::run_ptb(ptb, WITHDRAW_GAS_BUDGET)?;
    let digest = parse_digest(&out).unwrap_or_else(|| "(unknown)".into());
    println!("  ✓ tx {digest}");
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
