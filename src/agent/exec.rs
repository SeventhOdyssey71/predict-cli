//! Plan executor. Runs each leg through the existing `commands::trade::*`
//! write paths so every safety invariant (--max-cost, empty-manager-by-default,
//! coin auto-merge) is inherited unchanged.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::agent::plan::{Leg, Plan};
use crate::agent::store::{Position, PositionStatus, Store};
use crate::commands::trade::{self, MintBinary, MintRange, RedeemBinary, RedeemRange};

/// Execute every leg in a plan and persist the resulting position.
pub async fn execute(plan: Plan) -> Result<Position> {
    plan.validate()?;

    let mut position = Position::from_plan(plan);
    position.status = PositionStatus::Pending;

    println!("{}", "Executing plan".bold());
    println!("  intent     {}", position.id);
    println!("  view       {}", position.plan.directional_view);
    println!("  legs       {}", position.plan.legs.len());
    println!("  spend cap  ${:.2}", position.plan.max_total_spend_usdc);
    println!();

    for (idx, leg) in position.plan.legs.clone().iter().enumerate() {
        println!("{} leg {}", "→".dimmed(), idx + 1);
        match submit_leg(leg).await {
            Ok(()) => {
                position.mark_leg_submitted(idx);
            }
            Err(e) => {
                position.mark_leg(idx, format!("failed: {e}"));
                position.status = PositionStatus::Failed;
                save_or_warn(&position);
                return Err(e).context(format!("submitting leg {}", idx + 1));
            }
        }
    }

    position.status = PositionStatus::Open;
    save_or_warn(&position);
    Ok(position)
}

/// Redeem every still-open leg of a position, updating status to ClosedByUser.
pub async fn close(position: &mut Position) -> Result<()> {
    if matches!(
        position.status,
        PositionStatus::ClosedByUser | PositionStatus::Settled
    ) {
        anyhow::bail!("position {} is already {:?}", position.id, position.status);
    }

    println!("{} {}", "Closing".bold(), position.id);

    for (idx, leg) in position.plan.legs.clone().iter().enumerate() {
        let leg_state = position
            .legs
            .get(idx)
            .map(|l| l.status.clone())
            .unwrap_or_default();
        if leg_state == "redeemed" {
            continue;
        }
        match redeem_leg(leg, /* permissionless = */ false).await {
            Ok(()) => position.mark_leg_redeemed(idx),
            Err(e) => {
                position.mark_leg(idx, format!("redeem-failed: {e}"));
                save_or_warn(position);
                return Err(e).context(format!("redeeming leg {}", idx + 1));
            }
        }
    }

    position.status = PositionStatus::ClosedByUser;
    position.closed_at = Some(chrono::Utc::now());
    save_or_warn(position);
    Ok(())
}

pub(crate) async fn submit_leg(leg: &Leg) -> Result<()> {
    match leg {
        Leg::MintBinary {
            oracle_id,
            strike,
            side,
            quantity,
            deposit,
            max_cost,
            ..
        } => {
            trade::mint_binary(MintBinary {
                oracle_id: oracle_id.clone(),
                strike: *strike,
                is_up: side.is_up(),
                quantity: *quantity,
                deposit: *deposit,
                max_cost: *max_cost,
                allow_manager_balance: false,
            })
            .await
        }
        Leg::MintRange {
            oracle_id,
            lower,
            upper,
            quantity,
            deposit,
            max_cost,
            ..
        } => {
            trade::mint_range(MintRange {
                oracle_id: oracle_id.clone(),
                lower: *lower,
                upper: *upper,
                quantity: *quantity,
                deposit: *deposit,
                max_cost: *max_cost,
                allow_manager_balance: false,
            })
            .await
        }
    }
}

pub(crate) async fn redeem_leg(leg: &Leg, permissionless: bool) -> Result<()> {
    match leg {
        Leg::MintBinary {
            oracle_id,
            strike,
            side,
            quantity,
            ..
        } => {
            trade::redeem_binary(RedeemBinary {
                oracle_id: oracle_id.clone(),
                strike: *strike,
                is_up: side.is_up(),
                quantity: *quantity,
                permissionless,
            })
            .await
        }
        Leg::MintRange {
            oracle_id,
            lower,
            upper,
            quantity,
            ..
        } => {
            trade::redeem_range(RedeemRange {
                oracle_id: oracle_id.clone(),
                lower: *lower,
                upper: *upper,
                quantity: *quantity,
            })
            .await
        }
    }
}

fn save_or_warn(position: &Position) {
    if let Err(e) = persist(position) {
        eprintln!("warning: could not write position store: {e}");
    }
}

fn persist(position: &Position) -> Result<()> {
    let mut store = Store::load()?;
    store.upsert(position.clone());
    store.save()?;
    Ok(())
}
