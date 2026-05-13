//! Settlement daemon: poll the predict-server, redeem matured positions
//! according to each plan's exit policy.
//!
//! M2 surface: `predict-cli agent watch [--interval 30] [--once] [--only id]`.
//! No auto-roll (that's M3); when a leg is redeemed the position transitions to
//! `Settled` and the daemon stops touching it.
//!
//! Idempotency: leg state moves `pending → redeem_pending → redeemed` with a
//! store write between every transition. A crash mid-cycle leaves a
//! `redeem_pending` marker; on restart the daemon retries the same leg.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use owo_colors::OwoColorize;

use crate::agent::exec;
use crate::agent::plan::{ExitOnSettlement, Leg};
use crate::agent::roll;
use crate::agent::store::{Position, PositionStatus, Store};
use crate::server::{self, ServerOracle};

#[derive(Debug, Clone, Default)]
pub struct WatchConfig {
    pub interval: Duration,
    pub once: bool,
    pub only: Option<String>,
    pub json: bool,
    /// POST each CycleEvent as JSON to this URL.
    pub notify_webhook: Option<String>,
    /// Run this shell command for each CycleEvent. The event is injected as
    /// `EVENT_KIND`, `POSITION_ID`, `LEG_INDEX`, `DETAIL` env vars.
    pub notify_cmd: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CycleSummary {
    pub waiting: u32,
    pub redeemed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub events: Vec<CycleEvent>,
}

#[derive(Debug, serde::Serialize)]
pub struct CycleEvent {
    pub at: chrono::DateTime<Utc>,
    pub position_id: String,
    pub leg_index: usize,
    pub kind: String,
    pub detail: String,
}

/// What the daemon should do with a single leg this cycle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Wait,
    Redeem { permissionless: bool },
    AlreadyRedeemed,
    NoOracle,
}

/// Pure decision function: given a position, a leg index, and the live oracle
/// (if any), return what to do. Side-effect free; unit-tested directly.
pub(crate) fn decide(pos: &Position, leg_idx: usize, oracle: Option<&ServerOracle>) -> Decision {
    let leg_state = pos
        .legs
        .get(leg_idx)
        .map(|l| l.status.as_str())
        .unwrap_or("");
    if leg_state == "redeemed" {
        return Decision::AlreadyRedeemed;
    }

    let Some(oracle) = oracle else {
        return Decision::NoOracle;
    };

    let status = oracle.status.to_ascii_lowercase();
    if status == "active" {
        return Decision::Wait;
    }
    if status != "settled" {
        // Anything else (e.g. "compacted", "expired", unknown) — be conservative.
        return Decision::Wait;
    }

    match pos.plan.exit_policy.on_settlement {
        ExitOnSettlement::RedeemPermissionless => Decision::Redeem {
            permissionless: true,
        },
        ExitOnSettlement::RedeemOwner => Decision::Redeem {
            permissionless: false,
        },
        ExitOnSettlement::Hold => Decision::Wait,
    }
}

pub async fn run(cfg: WatchConfig) -> Result<()> {
    if !cfg.json {
        println!("{}", "agent watch".bold());
        println!("  interval   {}s", cfg.interval.as_secs().max(1));
        if let Some(only) = &cfg.only {
            println!("  only       {only}");
        }
        if cfg.notify_webhook.is_some() {
            println!("  webhook    on");
        }
        if cfg.notify_cmd.is_some() {
            println!("  notify cmd on");
        }
        if cfg.once {
            println!("  mode       single cycle");
        }
        println!();
    }

    loop {
        let summary = run_one_cycle(&cfg).await?;
        for ev in &summary.events {
            dispatch_notify(&cfg, ev).await;
        }
        emit_summary(&summary, cfg.json);

        if cfg.once {
            break;
        }
        tokio::time::sleep(cfg.interval).await;
    }
    Ok(())
}

async fn dispatch_notify(cfg: &WatchConfig, ev: &CycleEvent) {
    if let Some(url) = &cfg.notify_webhook {
        if let Err(e) = post_webhook(url, ev).await {
            eprintln!("notify webhook failed: {e}");
        }
    }
    if let Some(cmd) = &cfg.notify_cmd {
        if let Err(e) = run_notify_cmd(cmd, ev).await {
            eprintln!("notify cmd failed: {e}");
        }
    }
}

async fn post_webhook(url: &str, ev: &CycleEvent) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    client
        .post(url)
        .json(ev)
        .send()
        .await
        .context("posting webhook")?
        .error_for_status()
        .context("webhook returned error status")?;
    Ok(())
}

async fn run_notify_cmd(cmd: &str, ev: &CycleEvent) -> Result<()> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .env("EVENT_KIND", &ev.kind)
        .env("POSITION_ID", &ev.position_id)
        .env("LEG_INDEX", ev.leg_index.to_string())
        .env("DETAIL", &ev.detail);
    let status = command.status().await.context("spawning notify cmd")?;
    if !status.success() {
        anyhow::bail!("notify cmd exited with status {status}");
    }
    Ok(())
}

pub(crate) async fn run_one_cycle(cfg: &WatchConfig) -> Result<CycleSummary> {
    let mut summary = CycleSummary::default();

    let store = Store::load().context("loading position store")?;
    let positions: Vec<Position> = store
        .positions
        .iter()
        .filter(|p| matches!(p.status, PositionStatus::Open | PositionStatus::Pending))
        .filter(|p| match &cfg.only {
            Some(only) => &p.id == only,
            None => true,
        })
        .cloned()
        .collect();

    if positions.is_empty() {
        if cfg.only.is_some() {
            summary.events.push(CycleEvent {
                at: Utc::now(),
                position_id: cfg.only.clone().unwrap_or_default(),
                leg_index: 0,
                kind: "noop".into(),
                detail: "no matching open position".into(),
            });
        }
        return Ok(summary);
    }

    let oracles = server::list_oracles()
        .await
        .context("fetching predict-server oracles")?;
    let oracle_map: HashMap<&str, &ServerOracle> =
        oracles.iter().map(|o| (o.oracle_id.as_str(), o)).collect();

    for pos in positions {
        for (idx, leg) in pos.plan.legs.clone().iter().enumerate() {
            let oracle = oracle_map.get(leg.oracle_id()).copied();
            let decision = decide(&pos, idx, oracle);

            match decision {
                Decision::Wait => {
                    summary.waiting += 1;
                }
                Decision::AlreadyRedeemed => {
                    summary.skipped += 1;
                }
                Decision::NoOracle => {
                    summary.skipped += 1;
                    summary.events.push(CycleEvent {
                        at: Utc::now(),
                        position_id: pos.id.clone(),
                        leg_index: idx,
                        kind: "skip".into(),
                        detail: format!(
                            "oracle {} not in predict-server response",
                            leg.oracle_id()
                        ),
                    });
                }
                Decision::Redeem { permissionless } => {
                    match redeem_and_persist(&pos, idx, leg, permissionless).await {
                        Ok(()) => {
                            summary.redeemed += 1;
                            summary.events.push(CycleEvent {
                                at: Utc::now(),
                                position_id: pos.id.clone(),
                                leg_index: idx,
                                kind: "redeemed".into(),
                                detail: format!(
                                    "permissionless={permissionless}, oracle={}",
                                    leg.oracle_id()
                                ),
                            });

                            // M3: try auto-roll if the leg's policy says so.
                            match roll::try_roll(&pos.id, idx).await {
                                Ok(Some(outcome)) => {
                                    summary.events.push(CycleEvent {
                                        at: Utc::now(),
                                        position_id: pos.id.clone(),
                                        leg_index: outcome.new_leg_index,
                                        kind: "rolled".into(),
                                        detail: format!(
                                            "new leg submitted on oracle {}",
                                            outcome.oracle_id
                                        ),
                                    });
                                }
                                Ok(None) => { /* no roll fits; intentional */ }
                                Err(e) => {
                                    summary.failed += 1;
                                    summary.events.push(CycleEvent {
                                        at: Utc::now(),
                                        position_id: pos.id.clone(),
                                        leg_index: idx,
                                        kind: "error".into(),
                                        detail: format!("roll failed: {e}"),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            summary.failed += 1;
                            summary.events.push(CycleEvent {
                                at: Utc::now(),
                                position_id: pos.id.clone(),
                                leg_index: idx,
                                kind: "error".into(),
                                detail: format!("redeem failed: {e}"),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(summary)
}

async fn redeem_and_persist(
    pos: &Position,
    leg_idx: usize,
    leg: &Leg,
    permissionless: bool,
) -> Result<()> {
    // Step 1: mark redeem_pending so a crash mid-flight doesn't lose track.
    {
        let mut s = Store::load()?;
        if let Some(p) = s.find_mut(&pos.id) {
            p.mark_leg(leg_idx, "redeem_pending");
        }
        s.save()?;
    }

    exec::redeem_leg(leg, permissionless).await?;

    // Step 2: mark redeemed, transition position status if all legs done.
    let mut s = Store::load()?;
    if let Some(p) = s.find_mut(&pos.id) {
        p.mark_leg_redeemed(leg_idx);
        if p.legs.iter().all(|l| l.status == "redeemed") {
            p.status = PositionStatus::Settled;
            p.closed_at = Some(Utc::now());
        }
    }
    s.save()?;
    Ok(())
}

fn emit_summary(summary: &CycleSummary, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(&summary).unwrap_or_else(|_| "{}".into())
        );
        return;
    }

    let now = Utc::now().format("%H:%M:%S");
    if summary.waiting == 0 && summary.redeemed == 0 && summary.failed == 0 && summary.skipped == 0
    {
        println!("[{now}] no open positions");
        return;
    }
    println!(
        "[{now}] waiting={} redeemed={} failed={} skipped={}",
        summary.waiting, summary.redeemed, summary.failed, summary.skipped,
    );
    for ev in &summary.events {
        let kind = match ev.kind.as_str() {
            "redeemed" => "✓".green().to_string(),
            "error" => "✗".red().to_string(),
            "skip" => "·".dimmed().to_string(),
            _ => " ".to_string(),
        };
        println!(
            "  {} {} leg[{}] {}",
            kind, ev.position_id, ev.leg_index, ev.detail
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{
        ExitOnBudgetExhausted, ExitOnUserClose, ExitPolicy, Leg, Plan, RollingPolicy, Side,
    };
    use crate::agent::store::Position;

    fn sample_position(exit: ExitOnSettlement) -> Position {
        let plan = Plan {
            intent_id: "t1".into(),
            directional_view: "long BTC".into(),
            rationale: "test".into(),
            max_total_spend_usdc: 50.0,
            max_total_tenor_minutes: 60,
            legs: vec![Leg::MintBinary {
                oracle_id: "0xed5800".into(),
                strike: 82_000.0,
                side: Side::Up,
                quantity: 50.0,
                deposit: 35.0,
                max_cost: Some(40.0),
                rolling: RollingPolicy::None,
            }],
            exit_policy: ExitPolicy {
                on_settlement: exit,
                on_user_close: ExitOnUserClose::RedeemNow,
                on_budget_exhausted: ExitOnBudgetExhausted::Stop,
            },
        };
        Position::from_plan(plan)
    }

    fn server_oracle(status: &str) -> ServerOracle {
        ServerOracle {
            predict_id: "0xpredict".into(),
            oracle_id: "0xed5800".into(),
            oracle_cap_id: "0xcap".into(),
            underlying_asset: "BTC".into(),
            expiry: 1,
            min_strike: 0,
            tick_size: 1,
            status: status.into(),
            settlement_price: None,
            settled_at: None,
            activated_at: None,
            created_checkpoint: None,
        }
    }

    #[test]
    fn active_oracle_makes_us_wait() {
        let pos = sample_position(ExitOnSettlement::RedeemPermissionless);
        let o = server_oracle("active");
        assert_eq!(decide(&pos, 0, Some(&o)), Decision::Wait);
    }

    #[test]
    fn settled_oracle_with_permissionless_exit_redeems() {
        let pos = sample_position(ExitOnSettlement::RedeemPermissionless);
        let o = server_oracle("settled");
        assert_eq!(
            decide(&pos, 0, Some(&o)),
            Decision::Redeem {
                permissionless: true
            }
        );
    }

    #[test]
    fn settled_oracle_with_owner_exit_uses_owner_redeem() {
        let pos = sample_position(ExitOnSettlement::RedeemOwner);
        let o = server_oracle("settled");
        assert_eq!(
            decide(&pos, 0, Some(&o)),
            Decision::Redeem {
                permissionless: false
            }
        );
    }

    #[test]
    fn settled_oracle_with_hold_exit_does_nothing() {
        let pos = sample_position(ExitOnSettlement::Hold);
        let o = server_oracle("settled");
        assert_eq!(decide(&pos, 0, Some(&o)), Decision::Wait);
    }

    #[test]
    fn already_redeemed_leg_short_circuits() {
        let mut pos = sample_position(ExitOnSettlement::RedeemPermissionless);
        pos.mark_leg_redeemed(0);
        let o = server_oracle("settled");
        assert_eq!(decide(&pos, 0, Some(&o)), Decision::AlreadyRedeemed);
    }

    #[test]
    fn missing_oracle_skips_safely() {
        let pos = sample_position(ExitOnSettlement::RedeemPermissionless);
        assert_eq!(decide(&pos, 0, None), Decision::NoOracle);
    }

    #[test]
    fn unknown_status_falls_back_to_wait() {
        let pos = sample_position(ExitOnSettlement::RedeemPermissionless);
        let o = server_oracle("compacted");
        assert_eq!(decide(&pos, 0, Some(&o)), Decision::Wait);
    }
}
