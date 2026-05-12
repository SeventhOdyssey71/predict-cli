//! Structured intent → Plan. The non-LLM half of the planner.
//!
//! Takes the inputs `agent open --side --asset --tenor --risk` collects,
//! resolves a live oracle for the asset, picks a near-ATM strike from the
//! oracle's spot, and emits a single-leg Plan.

use anyhow::{anyhow, bail, Result};

use crate::agent::plan::{ExitPolicy, Leg, Plan, RollingPolicy, Side};
use crate::rpc::{pluck, u64_str, Rpc};
use crate::server::{self, ServerOracle};

/// What the user asked for at the structured CLI surface.
#[derive(Debug, Clone)]
pub struct StructuredIntent {
    pub side: IntentSide,
    pub asset: String,
    pub tenor_minutes: u64,
    pub risk_usdc: f64,
    pub tag: Option<String>,
    pub rolling: RollingPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentSide {
    Up,
    Down,
    Range,
}

/// Resolve the intent against live oracle state and produce a single-leg plan.
pub async fn intent_to_plan(intent: &StructuredIntent) -> Result<Plan> {
    if intent.risk_usdc <= 0.0 || !intent.risk_usdc.is_finite() {
        bail!(
            "--risk must be a positive finite number, got {}",
            intent.risk_usdc
        );
    }
    if intent.tenor_minutes == 0 {
        bail!("--tenor must be > 0 minutes");
    }

    let oracle = pick_oracle_for_asset(&intent.asset, intent.tenor_minutes).await?;
    let spot = read_spot(&oracle.oracle_id).await?;
    let strike = round_to_tick(spot, oracle.tick_size);

    // Deposit the full risk budget so the manager has room to absorb the
    // contract's all-in price (fair value + utilization fee). The mint will
    // abort if total cost exceeds `max_cost`, so we lose nothing by funding
    // 100% upfront. The 70/30 split we used in M1 left 30% un-deposited and
    // routinely tripped EBalanceManagerBalanceTooLow on real testnet mints.
    let deposit = (intent.risk_usdc * 100.0).round() / 100.0;
    let max_cost = deposit;

    // Quantity sized to fit the risk budget at ATM-binary pricing. For a
    // mid-price binary at ~0.5, qty * 0.5 ≈ deposit, leaving headroom for the
    // utilization fee. Deep-ITM mints (price near 1.0) intentionally fail
    // local validation downstream rather than burn the budget on near-certain
    // payouts.
    let quantity = ((deposit / 0.6) * 100.0).round() / 100.0;

    let leg = match intent.side {
        IntentSide::Up | IntentSide::Down => Leg::MintBinary {
            oracle_id: oracle.oracle_id.clone(),
            strike,
            side: if matches!(intent.side, IntentSide::Up) {
                Side::Up
            } else {
                Side::Down
            },
            quantity,
            deposit,
            max_cost: Some(max_cost),
            rolling: intent.rolling,
        },
        IntentSide::Range => {
            // Symmetric ±2% band around spot. M2 will let users tune this.
            let band = (spot * 0.02).max(oracle.tick_size as f64 / scale_factor());
            let lower = round_to_tick(spot - band, oracle.tick_size);
            let upper = round_to_tick(spot + band, oracle.tick_size);
            Leg::MintRange {
                oracle_id: oracle.oracle_id.clone(),
                lower,
                upper,
                quantity,
                deposit,
                max_cost: Some(max_cost),
                rolling: intent.rolling,
            }
        }
    };

    let intent_id = intent
        .tag
        .clone()
        .unwrap_or_else(crate::agent::store::auto_intent_id);

    let directional_view = match intent.side {
        IntentSide::Up => format!(
            "long {} above {} for {}m",
            intent.asset, strike, intent.tenor_minutes
        ),
        IntentSide::Down => format!(
            "short {} below {} for {}m",
            intent.asset, strike, intent.tenor_minutes
        ),
        IntentSide::Range => format!(
            "{} stays within ±2% of {} for {}m",
            intent.asset, spot, intent.tenor_minutes
        ),
    };

    let plan = Plan {
        intent_id,
        directional_view,
        rationale: format!(
            "structured intent: side={:?} asset={} tenor={}m risk=${} \
             oracle={} strike-policy=near-atm",
            intent.side, intent.asset, intent.tenor_minutes, intent.risk_usdc, oracle.oracle_id
        ),
        max_total_spend_usdc: max_cost,
        max_total_tenor_minutes: intent.tenor_minutes,
        legs: vec![leg],
        exit_policy: ExitPolicy::default(),
    };

    plan.validate()?;
    Ok(plan)
}

/// Pick the active oracle whose remaining tenor best matches `tenor_minutes`.
async fn pick_oracle_for_asset(asset: &str, tenor_minutes: u64) -> Result<ServerOracle> {
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let target_expiry = now_ms + tenor_minutes * 60_000;

    let oracles = server::list_oracles().await?;
    let asset_upper = asset.to_uppercase();

    let candidates: Vec<&ServerOracle> = oracles
        .iter()
        .filter(|o| {
            o.status.eq_ignore_ascii_case("active")
                && o.expiry > now_ms
                && o.underlying_asset.to_uppercase() == asset_upper
        })
        .collect();

    if candidates.is_empty() {
        bail!(
            "no active oracle for asset `{}` (try: `predict-cli list`)",
            asset
        );
    }

    let chosen = candidates
        .into_iter()
        .min_by_key(|o| (o.expiry as i64 - target_expiry as i64).unsigned_abs())
        .ok_or_else(|| anyhow!("no candidate oracle"))?;

    Ok(chosen.clone())
}

/// Pull current spot from the oracle's on-chain state. The on-chain layout
/// nests price fields under `prices.fields.spot`, matching the same path
/// `commands::oracle::run` uses for the human view.
pub(crate) async fn read_spot(oracle_id: &str) -> Result<f64> {
    let rpc = Rpc::new();
    let obj = rpc.get_object(oracle_id).await?;
    let fields = pluck(&obj, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("oracle {oracle_id} has no fields"))?;

    let prices = pluck(fields, &["prices", "fields"])
        .ok_or_else(|| anyhow!("oracle {oracle_id} has no prices field"))?;
    let spot_scaled = u64_str(prices.get("spot"));
    if spot_scaled == 0 {
        bail!("oracle {oracle_id} reports spot = 0; cannot pick strike");
    }
    Ok(spot_scaled as f64 / scale_factor())
}

pub(crate) fn round_to_tick(price: f64, tick_scaled: u64) -> f64 {
    if tick_scaled == 0 {
        return price;
    }
    let tick = tick_scaled as f64 / scale_factor();
    if tick <= 0.0 {
        return price;
    }
    (price / tick).round() * tick
}

pub(crate) fn scale_factor() -> f64 {
    // FLOAT_SCALING is 1e9; on-chain prices are scaled by it.
    1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_to_tick_snaps() {
        // tick = 100 (with 1e9 scaling, that's 100 / 1e9 of the unit, but for
        // the test we just confirm rounding behavior on real values).
        // Use a tick_scaled value that maps to a $1 grid: 1.0 * 1e9 = 1e9.
        assert!((round_to_tick(82_257.4, 1_000_000_000) - 82_257.0).abs() < 1e-9);
        assert!((round_to_tick(82_257.6, 1_000_000_000) - 82_258.0).abs() < 1e-9);
    }

    #[test]
    fn round_to_tick_zero_passes_through() {
        assert!((round_to_tick(82_257.4, 0) - 82_257.4).abs() < 1e-9);
    }
}
