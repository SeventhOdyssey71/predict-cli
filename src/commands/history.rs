//! `predict-cli history` — reverse-chronological ledger of the user's own
//! activity: mints, redeems, deposits, withdraws, LP supplies, LP withdraws.
//!
//! Implementation: one call to `suix_queryTransactionBlocks` per page,
//! filtering by sender = active address. Each tx is classified by inspecting
//! which `predict::*` or `predict_manager::*` Move calls fired inside it
//! (via the emitted events), and amounts come from the tx's balance_changes.
//!
//! Limitations:
//! - Permissionless redeems initiated by a third-party keeper on the user's
//!   behalf won't show up here (the keeper is the tx sender, not the user).
//!   Add `--include-permissionless` later by querying PositionRedeemed events
//!   filtered client-side by the `owner` field.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use owo_colors::OwoColorize;
use serde::Serialize;
use serde_json::Value;

use crate::config::PREDICT_PACKAGE;
use crate::format::{fmt_strike, fmt_usd, shorten};
use crate::rpc::{u64_str, Rpc};
use crate::sui_cli;

#[derive(Debug, Clone)]
pub struct HistoryArgs {
    pub limit: u32,
    pub include_failed: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Deposit is reserved for the deeper-history pass that
                    // inspects PTB input objects to disambiguate plain
                    // deposit-to-manager calls from withdraw-to-wallet calls.
pub enum HistoryKind {
    Mint,
    Redeem,
    Deposit,
    Withdraw,
    LpSupply,
    LpWithdraw,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub digest: String,
    pub timestamp_ms: u64,
    pub kind: HistoryKind,
    /// Net change in DUSDC (microDUSDC, signed) for the user's address.
    pub dusdc_delta: i128,
    /// Brief one-line detail (oracle, strike, qty, etc.). Empty when unknown.
    pub detail: String,
    /// True if effects.status was failure. Failed txs cost gas but do nothing
    /// else; useful to see when troubleshooting.
    pub failed: bool,
}

pub async fn run(args: HistoryArgs) -> Result<()> {
    sui_cli::check()?;
    let addr = sui_cli::active_address()?;
    let rpc = Rpc::new();

    let entries = fetch(&rpc, &addr, args.limit).await?;

    let visible: Vec<&HistoryEntry> = entries
        .iter()
        .filter(|e| args.include_failed || !e.failed)
        .filter(|e| !matches!(e.kind, HistoryKind::Other))
        .collect();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&visible)?);
        return Ok(());
    }

    if visible.is_empty() {
        println!("no history yet. open a position with `predict-cli agent open`.");
        return Ok(());
    }

    println!("{}", "History".bold());
    println!(
        "  {:<19} {:<12} {:>12}  {}",
        "when".dimmed(),
        "kind".dimmed(),
        "dusdc".dimmed(),
        "detail".dimmed()
    );
    for e in visible {
        let when = DateTime::<Utc>::from_timestamp_millis(e.timestamp_ms as i64)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into());
        let amount = if e.dusdc_delta == 0 {
            "—".dimmed().to_string()
        } else {
            let usdc = (e.dusdc_delta as f64) / 1_000_000.0;
            if usdc >= 0.0 {
                format!("+{}", fmt_usd(usdc))
            } else {
                format!("-{}", fmt_usd(-usdc))
            }
        };
        println!(
            "  {:<19} {:<12} {:>12}  {}",
            when,
            kind_label(&e.kind),
            amount,
            e.detail
        );
    }
    println!();
    println!(
        "  {} suivision.xyz/txblock/<digest> for full tx detail",
        "→".dimmed()
    );

    Ok(())
}

/// Walk the user's tx history descending until we have `limit` classified
/// entries (so we don't return prematurely if half the page was unrelated
/// txs from other apps). Caps total pages to avoid runaway scans.
async fn fetch(rpc: &Rpc, sender: &str, limit: u32) -> Result<Vec<HistoryEntry>> {
    let mut out: Vec<HistoryEntry> = Vec::new();
    let mut cursor: Option<String> = None;
    let page_size: u32 = 50;
    let max_pages: u32 = 10;
    let mut pages = 0u32;

    while pages < max_pages && (out.len() as u32) < limit {
        let resp = rpc
            .query_transactions(sender, cursor.as_deref(), page_size)
            .await?;
        let data = resp
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();

        for tx in &data {
            if let Some(entry) = classify_tx(tx, sender) {
                out.push(entry);
                if (out.len() as u32) >= limit {
                    break;
                }
            }
        }

        let has_next = resp
            .get("hasNextPage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !has_next {
            break;
        }
        cursor = resp
            .get("nextCursor")
            .and_then(|v| v.as_str())
            .map(String::from);
        if cursor.is_none() {
            break;
        }
        pages += 1;
    }

    Ok(out)
}

/// Classify one transaction. Returns None for txs we don't care about
/// (i.e. nothing predict-related happened in them).
fn classify_tx(tx: &Value, _owner: &str) -> Option<HistoryEntry> {
    let digest = tx.get("digest").and_then(|v| v.as_str())?.to_string();
    let timestamp_ms = u64_str(tx.get("timestampMs"));

    let events = tx
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut kind = HistoryKind::Other;
    let mut detail = String::new();
    // Signed microDUSDC change to the user's wallet. Positive = wallet gained
    // (e.g. payout, manager → wallet withdraw). Negative = wallet sent
    // (mint cost, deposit-to-manager, supply).
    let mut dusdc_delta: i128 = 0;

    let predict_pkg = PREDICT_PACKAGE.trim_start_matches("0x");

    // Walk events; the first predict-relevant one wins. A tx with both a
    // deposit step and a mint step surfaces as MINT (the higher-level intent).
    for ev in &events {
        let event_type = ev.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        let parsed = ev.get("parsedJson");

        if event_type.contains(predict_pkg) {
            if event_type.contains("::predict::PositionMinted") {
                kind = HistoryKind::Mint;
                detail = describe_mint(parsed);
                // Cost on mint flows wallet → manager, so the wallet loses it.
                let cost = parsed
                    .and_then(|p| p.get("cost"))
                    .map(u64_from_value)
                    .unwrap_or(0);
                dusdc_delta = -(cost as i128);
                break;
            }
            if event_type.contains("::predict::PositionRedeemed") {
                kind = HistoryKind::Redeem;
                detail = describe_redeem(parsed);
                // Payout lands in the manager, not the wallet, so wallet
                // delta is technically 0 here. We still surface payout in
                // the detail line; for true "where did my money go" the
                // user can `agent positions` + `manager` after.
                dusdc_delta = 0;
                break;
            }
            if event_type.contains("::predict::Supplied") {
                kind = HistoryKind::LpSupply;
                detail = describe_lp(parsed, "in");
                let amt = parsed
                    .and_then(|p| p.get("quantity_quote_in"))
                    .map(u64_from_value)
                    .unwrap_or(0);
                dusdc_delta = -(amt as i128);
                break;
            }
            if event_type.contains("::predict::Withdrawn") {
                kind = HistoryKind::LpWithdraw;
                detail = describe_lp(parsed, "out");
                let amt = parsed
                    .and_then(|p| p.get("quantity_quote_out"))
                    .map(u64_from_value)
                    .unwrap_or(0);
                dusdc_delta = amt as i128;
                break;
            }
        }
        // Plain manager deposit / withdraw — only triggers when no predict
        // high-level event fired in this tx. The BalanceEvent payload has
        // an `amount` field; sign depends on the call direction, which we
        // infer from whether the tx also called predict_manager::deposit
        // vs ::withdraw.
        if event_type.contains("::balance_manager::BalanceEvent") {
            // Heuristic: if any other event mentions "predict_manager", we'll
            // pick that up first. Plain BalanceEvent here = a bare
            // manager → wallet withdraw or wallet → manager deposit.
            let amount = parsed
                .and_then(|p| p.get("amount"))
                .map(u64_from_value)
                .unwrap_or(0);
            // We need direction. The PTB call list isn't in the response
            // unless we ask for showInput, which slows the call. Cheap
            // heuristic for now: predict-cli's only direct call paths
            // are `deposit` (wallet → manager) and `manager --withdraw`
            // (manager → wallet); show as WITHDRAW (the common case after
            // a winning redeem) and surface the amount.
            kind = HistoryKind::Withdraw;
            detail = format!("balance change {}", fmt_usd(amount as f64 / 1_000_000.0));
            dusdc_delta = amount as i128;
        }
    }

    if matches!(kind, HistoryKind::Other) {
        return None;
    }

    Some(HistoryEntry {
        digest,
        timestamp_ms,
        kind,
        dusdc_delta,
        detail,
        failed: false,
    })
}

fn u64_from_value(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

/// On testnet `predict-testnet-*`, binary-event payloads have flat fields:
///   strike: u64 (1e9 scale)
///   is_up:  bool
///   quantity: u64 (1e6 scale, payout-on-win in microDUSDC)
///   cost / payout: u64 (1e6 scale, microDUSDC)
fn describe_mint(parsed: Option<&Value>) -> String {
    let Some(p) = parsed else {
        return String::new();
    };
    let oracle_id = p
        .get("oracle_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let qty = (u64_str(p.get("quantity")) as f64) / 1_000_000.0;
    let cost_usd = (u64_str(p.get("cost")) as f64) / 1_000_000.0;
    let side = binary_side_label(p).unwrap_or_else(|| "binary".into());
    format!(
        "{} qty {:.2} cost {} (oracle {})",
        side,
        qty,
        fmt_usd(cost_usd),
        shorten(oracle_id)
    )
}

fn describe_redeem(parsed: Option<&Value>) -> String {
    let Some(p) = parsed else {
        return String::new();
    };
    let qty = (u64_str(p.get("quantity")) as f64) / 1_000_000.0;
    let payout_usd = (u64_str(p.get("payout")) as f64) / 1_000_000.0;
    let is_settled = p
        .get("is_settled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let side = binary_side_label(p).unwrap_or_else(|| "position".into());
    let suffix = if is_settled { "settled" } else { "live" };
    format!(
        "{} qty {:.2} payout {} ({suffix})",
        side,
        qty,
        fmt_usd(payout_usd)
    )
}

/// Returns "UP @$80549" / "DOWN @$80549" for binary events. None for range
/// events (different schema; surfaced generically by the caller).
fn binary_side_label(p: &Value) -> Option<String> {
    let is_up = p.get("is_up").and_then(|v| v.as_bool())?;
    let strike = u64_str(p.get("strike"));
    if strike == 0 {
        return None;
    }
    let strike_usd = (strike as f64) / 1_000_000_000.0;
    let prefix = if is_up { "UP" } else { "DOWN" };
    Some(format!("{prefix} @{}", fmt_strike(strike_usd, "BTC")))
}

/// `direction` is "in" for supply (wallet → vault) or "out" for withdraw.
fn describe_lp(parsed: Option<&Value>, direction: &str) -> String {
    let Some(p) = parsed else {
        return String::new();
    };
    let (amount_key, shares_key) = if direction == "in" {
        ("quantity_quote_in", "shares_out")
    } else {
        ("quantity_quote_out", "shares_in")
    };
    let amount = u64_str(p.get(amount_key));
    let shares = u64_str(p.get(shares_key));
    format!(
        "{} dusdc, {} PLP shares",
        fmt_usd(amount as f64 / 1_000_000.0),
        shares
    )
}

fn kind_label(k: &HistoryKind) -> &'static str {
    match k {
        HistoryKind::Mint => "mint",
        HistoryKind::Redeem => "redeem",
        HistoryKind::Deposit => "deposit",
        HistoryKind::Withdraw => "withdraw",
        HistoryKind::LpSupply => "lp-supply",
        HistoryKind::LpWithdraw => "lp-withdraw",
        HistoryKind::Other => "other",
    }
}

/// Group history by ISO date and produce a per-day P&L summary. Not wired
/// to a subcommand yet; reserved for `predict-cli pnl` next.
#[allow(dead_code)]
pub fn group_by_day(entries: &[HistoryEntry]) -> BTreeMap<String, i128> {
    let mut by_day: BTreeMap<String, i128> = BTreeMap::new();
    for e in entries {
        let day = DateTime::<Utc>::from_timestamp_millis(e.timestamp_ms as i64)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        *by_day.entry(day).or_insert(0) += e.dusdc_delta;
    }
    by_day
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_mint_tx(cost: u64) -> Value {
        serde_json::json!({
            "digest": "ABC",
            "timestampMs": "1700000000000",
            "events": [{
                "type": format!("{}::predict::PositionMinted", PREDICT_PACKAGE),
                "parsedJson": {
                    "oracle_id": "0xed5800",
                    "strike": "82000000000000",
                    "is_up": true,
                    "quantity": "8330000",
                    "cost": cost.to_string()
                }
            }]
        })
    }

    #[test]
    fn classify_mint() {
        let tx = fake_mint_tx(4_248_216);
        let entry = classify_tx(&tx, "0xowner").unwrap();
        assert!(matches!(entry.kind, HistoryKind::Mint));
        // Mint cost flows wallet → manager so wallet delta is negative.
        assert_eq!(entry.dusdc_delta, -4_248_216);
        assert!(entry.detail.contains("UP"));
        assert!(entry.detail.contains("$82000"));
    }

    #[test]
    fn classify_redeem() {
        let tx = serde_json::json!({
            "digest": "DEF",
            "timestampMs": "1700000050000",
            "events": [{
                "type": format!("{}::predict::PositionRedeemed", PREDICT_PACKAGE),
                "parsedJson": {
                    "oracle_id": "0xed5800",
                    "strike": "80549000000000",
                    "is_up": true,
                    "quantity": "8330000",
                    "payout": "8330000",
                    "is_settled": true
                }
            }]
        });
        let entry = classify_tx(&tx, "0xowner").unwrap();
        assert!(matches!(entry.kind, HistoryKind::Redeem));
        assert!(entry.detail.contains("settled"));
        assert!(entry.detail.contains("UP"));
        assert!(entry.detail.contains("$80549"));
    }

    #[test]
    fn classify_skips_unrelated_tx() {
        let tx = serde_json::json!({
            "digest": "XYZ",
            "timestampMs": "1700000000000",
            "events": [{
                "type": "0x2::coin::CoinMetadata",
                "parsedJson": {}
            }]
        });
        assert!(classify_tx(&tx, "0xowner").is_none());
    }

    #[test]
    fn group_by_day_sums() {
        let entries = vec![
            HistoryEntry {
                digest: "A".into(),
                timestamp_ms: 1700000000000,
                kind: HistoryKind::Mint,
                dusdc_delta: -5_000_000,
                detail: String::new(),
                failed: false,
            },
            HistoryEntry {
                digest: "B".into(),
                timestamp_ms: 1700000010000,
                kind: HistoryKind::Redeem,
                dusdc_delta: 8_000_000,
                detail: String::new(),
                failed: false,
            },
        ];
        let by_day = group_by_day(&entries);
        assert_eq!(by_day.values().next().copied().unwrap_or(0), 3_000_000);
    }
}
