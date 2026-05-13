//! Auto-roll: when a leg redeems and its `RollingPolicy` is `AutoOnSettlement`,
//! plan a fresh leg into the next active oracle for the same asset and submit
//! it. Bounded by the position's remaining budget and remaining tenor.
//!
//! No new public CLI surface. The watch daemon calls `try_roll` after a
//! successful redeem, and the new leg is appended to the position's plan.

use anyhow::Result;
use chrono::Utc;

use crate::agent::exec;
use crate::agent::intent::{read_spot, round_to_tick, scale_factor};
use crate::agent::plan::{Leg, RollingPolicy, Side};
use crate::agent::store::{LegRecord, PositionStatus, Store};
use crate::server::{self, ServerOracle};

/// Below this remaining budget, we don't bother rolling.
const MIN_ROLL_DEPOSIT_USDC: f64 = 1.0;
/// Below this remaining tenor, we don't bother rolling either.
const MIN_REMAINING_TENOR_MIN: u64 = 5;

#[derive(Debug, Clone)]
pub struct RollOutcome {
    pub new_leg_index: usize,
    pub oracle_id: String,
}

/// Decide whether the position should roll, and if so, build + submit the next
/// leg. Returns `Ok(None)` if no roll fits (budget exhausted, tenor exhausted,
/// no candidate oracle, or rolling policy off).
pub async fn try_roll(position_id: &str, redeemed_leg_idx: usize) -> Result<Option<RollOutcome>> {
    let store = Store::load()?;
    let position = match store.find(position_id) {
        Some(p) => p.clone(),
        None => return Ok(None),
    };

    let redeemed_leg = match position.plan.legs.get(redeemed_leg_idx) {
        Some(l) => l.clone(),
        None => return Ok(None),
    };

    if redeemed_leg.rolling() != RollingPolicy::AutoOnSettlement {
        return Ok(None);
    }

    // Budget remaining: original cap minus sum of all leg deposits so far.
    let already_committed: f64 = position.plan.legs.iter().map(Leg::deposit).sum();
    let remaining_budget = position.plan.max_total_spend_usdc - already_committed;
    if remaining_budget < MIN_ROLL_DEPOSIT_USDC {
        return Ok(None);
    }

    // Tenor remaining: original tenor minus elapsed since open.
    let elapsed_minutes = (Utc::now() - position.opened_at).num_minutes().max(0) as u64;
    let remaining_tenor = position
        .plan
        .max_total_tenor_minutes
        .saturating_sub(elapsed_minutes);
    if remaining_tenor < MIN_REMAINING_TENOR_MIN {
        return Ok(None);
    }

    // Find the next active oracle for the same asset, distinct from the one we
    // just redeemed, with expiry within the remaining tenor.
    let oracles = server::list_oracles().await?;
    let asset = match oracles
        .iter()
        .find(|o| o.oracle_id == redeemed_leg.oracle_id())
        .map(|o| o.underlying_asset.clone())
    {
        Some(a) => a,
        None => return Ok(None),
    };

    let now_ms = Utc::now().timestamp_millis() as u64;
    let max_expiry = now_ms + remaining_tenor * 60_000;
    let candidate = pick_next_oracle(
        &oracles,
        &asset,
        redeemed_leg.oracle_id(),
        now_ms,
        max_expiry,
    );
    let Some(next_oracle) = candidate else {
        return Ok(None);
    };

    // Build the new leg as a mirror of the redeemed leg, snapped to the new
    // oracle's tick grid and current spot. Deposit is the smaller of the
    // remaining budget and the original leg's deposit (so we don't escalate).
    let spot = read_spot(&next_oracle.oracle_id).await?;
    let original_deposit = redeemed_leg.deposit();
    let deposit = remaining_budget.min(original_deposit);
    let max_cost = remaining_budget;

    let new_leg = build_rolled_leg(&redeemed_leg, next_oracle, spot, deposit, max_cost);

    // Persist the appended leg as pending before submission so a crash
    // mid-submit leaves a discoverable record.
    {
        let mut s = Store::load()?;
        if let Some(p) = s.find_mut(position_id) {
            p.plan.legs.push(new_leg.clone());
            p.legs.push(LegRecord {
                leg_index: p.plan.legs.len() - 1,
                status: "roll_pending".into(),
                submitted_at: None,
                redeemed_at: None,
            });
        }
        s.save()?;
    }

    // Submit. On failure we record the error and bail; the leg stays in
    // `roll_pending` and the daemon will retry on the next cycle.
    if let Err(e) = exec::submit_leg(&new_leg).await {
        let mut s = Store::load()?;
        if let Some(p) = s.find_mut(position_id) {
            let last_idx = p.legs.len().saturating_sub(1);
            p.mark_leg(last_idx, format!("roll_failed: {e}"));
        }
        s.save()?;
        return Err(e);
    }

    let mut s = Store::load()?;
    let new_leg_index = if let Some(p) = s.find_mut(position_id) {
        let idx = p.legs.len().saturating_sub(1);
        p.mark_leg_submitted(idx);
        // The position remains Open; don't transition status. The watch loop
        // will redeem this new leg when it settles.
        p.status = PositionStatus::Open;
        idx
    } else {
        return Ok(None);
    };
    s.save()?;

    Ok(Some(RollOutcome {
        new_leg_index,
        oracle_id: next_oracle.oracle_id.clone(),
    }))
}

/// Pure: pick the next active oracle for `asset`, excluding the just-redeemed
/// one, that expires no later than `max_expiry`. Among candidates, we pick the
/// nearest expiry > `now_ms` so consecutive rolls keep the position rolling
/// "into the next bar".
pub(crate) fn pick_next_oracle<'a>(
    oracles: &'a [ServerOracle],
    asset: &str,
    exclude_oracle_id: &str,
    now_ms: u64,
    max_expiry_ms: u64,
) -> Option<&'a ServerOracle> {
    let asset_upper = asset.to_uppercase();
    oracles
        .iter()
        .filter(|o| {
            o.status.eq_ignore_ascii_case("active")
                && o.oracle_id != exclude_oracle_id
                && o.underlying_asset.to_uppercase() == asset_upper
                && o.expiry > now_ms
                && o.expiry <= max_expiry_ms
        })
        .min_by_key(|o| o.expiry)
}

fn build_rolled_leg(
    original: &Leg,
    oracle: &ServerOracle,
    spot: f64,
    deposit: f64,
    max_cost: f64,
) -> Leg {
    match original {
        Leg::MintBinary { side, quantity, .. } => Leg::MintBinary {
            oracle_id: oracle.oracle_id.clone(),
            strike: round_to_tick(spot, oracle.tick_size),
            side: *side,
            quantity: *quantity,
            deposit,
            max_cost: Some(max_cost),
            rolling: RollingPolicy::AutoOnSettlement,
        },
        Leg::MintRange {
            quantity,
            lower,
            upper,
            ..
        } => {
            // Preserve the band width relative to the new spot.
            let original_band =
                ((upper - lower) / 2.0).max(oracle.tick_size as f64 / scale_factor());
            let lo = round_to_tick(spot - original_band, oracle.tick_size);
            let hi = round_to_tick(spot + original_band, oracle.tick_size);
            Leg::MintRange {
                oracle_id: oracle.oracle_id.clone(),
                lower: lo,
                upper: hi,
                quantity: *quantity,
                deposit,
                max_cost: Some(max_cost),
                rolling: RollingPolicy::AutoOnSettlement,
            }
        }
    }
}

#[allow(dead_code)]
fn _side_compile_check(s: Side) -> bool {
    s.is_up()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle(id: &str, asset: &str, status: &str, expiry_ms: u64) -> ServerOracle {
        ServerOracle {
            predict_id: "0xpredict".into(),
            oracle_id: id.into(),
            oracle_cap_id: "0xcap".into(),
            underlying_asset: asset.into(),
            expiry: expiry_ms,
            min_strike: 0,
            tick_size: 1_000_000_000,
            status: status.into(),
            settlement_price: None,
            settled_at: None,
            activated_at: None,
            created_checkpoint: None,
        }
    }

    #[test]
    fn picks_nearest_active_excluding_current() {
        let now_ms = 1_000_000_000;
        let max = now_ms + 60 * 60_000;
        let oracles = vec![
            oracle("0xCURRENT", "BTC", "settled", now_ms - 1),
            oracle("0xNEXT", "BTC", "active", now_ms + 30 * 60_000),
            oracle("0xLATER", "BTC", "active", now_ms + 50 * 60_000),
        ];
        let pick = pick_next_oracle(&oracles, "BTC", "0xCURRENT", now_ms, max);
        assert_eq!(pick.map(|o| o.oracle_id.as_str()), Some("0xNEXT"));
    }

    #[test]
    fn skips_oracles_past_max_expiry() {
        let now_ms = 1_000_000_000;
        let max = now_ms + 60 * 60_000;
        let oracles = vec![
            oracle("0xCURRENT", "BTC", "settled", now_ms - 1),
            oracle("0xWAY_OUT", "BTC", "active", now_ms + 120 * 60_000),
        ];
        assert!(pick_next_oracle(&oracles, "BTC", "0xCURRENT", now_ms, max).is_none());
    }

    #[test]
    fn skips_wrong_asset() {
        let now_ms = 1_000_000_000;
        let max = now_ms + 60 * 60_000;
        let oracles = vec![oracle("0xETH_NEXT", "ETH", "active", now_ms + 30 * 60_000)];
        assert!(pick_next_oracle(&oracles, "BTC", "0xPREV", now_ms, max).is_none());
    }

    #[test]
    fn case_insensitive_asset_match() {
        let now_ms = 1_000_000_000;
        let max = now_ms + 60 * 60_000;
        let oracles = vec![oracle("0xNEXT", "btc", "active", now_ms + 10 * 60_000)];
        let pick = pick_next_oracle(&oracles, "BTC", "0xPREV", now_ms, max);
        assert_eq!(pick.map(|o| o.oracle_id.as_str()), Some("0xNEXT"));
    }
}
