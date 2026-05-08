//! Plan = a validated, machine-checkable description of what the agent will do.
//!
//! Every surface (structured DSL, LLM, mobile-app API) emits the same shape.
//! The executor only ever reads a `Plan`; it never reads user intent directly.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Up,
    Down,
}

impl Side {
    pub fn is_up(self) -> bool {
        matches!(self, Side::Up)
    }
}

/// Strike-selection policies. M1 supports near-ATM only; richer policies
/// (1σ, dynamic on realized vol) arrive with the LLM planner in M4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StrikePolicy {
    /// Snap to the closest mintable strike to current spot.
    NearAtm,
    /// User picked an exact strike upstream.
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingPolicy {
    /// Single-shot: leg expires, position closes.
    None,
    /// Roll into the next expiry on settlement (M3+).
    AutoOnSettlement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitOnSettlement {
    RedeemPermissionless,
    RedeemOwner,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitOnUserClose {
    RedeemNow,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitOnBudgetExhausted {
    Stop,
    Continue,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExitPolicy {
    pub on_settlement: ExitOnSettlement,
    pub on_user_close: ExitOnUserClose,
    pub on_budget_exhausted: ExitOnBudgetExhausted,
}

impl Default for ExitPolicy {
    fn default() -> Self {
        Self {
            on_settlement: ExitOnSettlement::RedeemPermissionless,
            on_user_close: ExitOnUserClose::RedeemNow,
            on_budget_exhausted: ExitOnBudgetExhausted::Stop,
        }
    }
}

/// One write the executor will perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Leg {
    MintBinary {
        oracle_id: String,
        strike: f64,
        side: Side,
        quantity: f64,
        deposit: f64,
        max_cost: Option<f64>,
        rolling: RollingPolicy,
    },
    MintRange {
        oracle_id: String,
        lower: f64,
        upper: f64,
        quantity: f64,
        deposit: f64,
        max_cost: Option<f64>,
        rolling: RollingPolicy,
    },
}

impl Leg {
    pub fn deposit(&self) -> f64 {
        match self {
            Leg::MintBinary { deposit, .. } | Leg::MintRange { deposit, .. } => *deposit,
        }
    }

    #[allow(dead_code)]
    pub fn oracle_id(&self) -> &str {
        match self {
            Leg::MintBinary { oracle_id, .. } | Leg::MintRange { oracle_id, .. } => oracle_id,
        }
    }
}

/// What the user asked for, what the agent will do, and the bounds on doing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// Stable id chosen by the user (`--tag`) or auto-generated.
    pub intent_id: String,
    /// Free-text description of the user's view (for the position list / inspect).
    pub directional_view: String,
    /// Why the planner chose these legs. Populated by LLM planners; structured
    /// DSL leaves a one-line rationale here.
    pub rationale: String,
    /// Hard ceiling on total deposits across all legs.
    pub max_total_spend_usdc: f64,
    /// Hard ceiling on the lifetime of the position (informational at M1; the
    /// daemon will enforce it at M2+).
    pub max_total_tenor_minutes: u64,
    pub legs: Vec<Leg>,
    pub exit_policy: ExitPolicy,
}

impl Plan {
    /// Total declared deposit across all legs.
    pub fn total_declared_deposit(&self) -> f64 {
        self.legs.iter().map(Leg::deposit).sum()
    }

    /// Validate the plan's internal invariants. The executor must call this
    /// before the first write.
    pub fn validate(&self) -> Result<()> {
        if self.intent_id.is_empty() {
            bail!("plan: intent_id is required");
        }
        if !self
            .intent_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            bail!("plan: intent_id must be ascii alphanumeric, '-', '_', or '.'");
        }
        if self.legs.is_empty() {
            bail!("plan: at least one leg is required");
        }
        if !self.max_total_spend_usdc.is_finite() || self.max_total_spend_usdc <= 0.0 {
            bail!("plan: max_total_spend_usdc must be a positive finite number");
        }
        if self.max_total_tenor_minutes == 0 {
            bail!("plan: max_total_tenor_minutes must be > 0");
        }

        for (i, leg) in self.legs.iter().enumerate() {
            validate_leg(leg).map_err(|e| anyhow::anyhow!("plan: leg[{i}]: {e}"))?;
        }

        let total = self.total_declared_deposit();
        if total > self.max_total_spend_usdc + f64::EPSILON {
            bail!(
                "plan: sum of leg deposits ({total:.4}) exceeds max_total_spend_usdc ({:.4})",
                self.max_total_spend_usdc
            );
        }

        Ok(())
    }
}

fn validate_leg(leg: &Leg) -> Result<()> {
    match leg {
        Leg::MintBinary {
            oracle_id,
            strike,
            quantity,
            deposit,
            max_cost,
            ..
        } => {
            require_oracle_id(oracle_id)?;
            require_pos("strike", *strike)?;
            require_pos("quantity", *quantity)?;
            require_pos("deposit", *deposit)?;
            if let Some(m) = max_cost {
                require_pos("max_cost", *m)?;
                if *m < *deposit {
                    bail!("max_cost ({m}) must be >= deposit ({deposit})");
                }
            }
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
            require_oracle_id(oracle_id)?;
            require_pos("lower", *lower)?;
            require_pos("upper", *upper)?;
            if upper <= lower {
                bail!("upper ({upper}) must be strictly greater than lower ({lower})");
            }
            require_pos("quantity", *quantity)?;
            require_pos("deposit", *deposit)?;
            if let Some(m) = max_cost {
                require_pos("max_cost", *m)?;
                if *m < *deposit {
                    bail!("max_cost ({m}) must be >= deposit ({deposit})");
                }
            }
        }
    }
    Ok(())
}

fn require_oracle_id(s: &str) -> Result<()> {
    if !s.starts_with("0x") || s.len() < 4 {
        bail!("oracle_id must be a 0x-prefixed Sui object id, got `{s}`");
    }
    Ok(())
}

fn require_pos(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v <= 0.0 {
        bail!("{name} must be a positive finite number, got {v}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_leg(deposit: f64) -> Leg {
        Leg::MintBinary {
            oracle_id: "0xabcdef0123".into(),
            strike: 82_000.0,
            side: Side::Up,
            quantity: 50.0,
            deposit,
            max_cost: Some(deposit + 5.0),
            rolling: RollingPolicy::None,
        }
    }

    fn range_leg(lower: f64, upper: f64) -> Leg {
        Leg::MintRange {
            oracle_id: "0xabcdef0123".into(),
            lower,
            upper,
            quantity: 50.0,
            deposit: 10.0,
            max_cost: None,
            rolling: RollingPolicy::None,
        }
    }

    fn good_plan() -> Plan {
        Plan {
            intent_id: "morning-bias".into(),
            directional_view: "long BTC".into(),
            rationale: "structured open".into(),
            max_total_spend_usdc: 50.0,
            max_total_tenor_minutes: 60,
            legs: vec![binary_leg(35.0)],
            exit_policy: ExitPolicy::default(),
        }
    }

    #[test]
    fn good_plan_validates() {
        good_plan().validate().unwrap();
    }

    #[test]
    fn empty_legs_rejected() {
        let mut p = good_plan();
        p.legs.clear();
        assert!(p.validate().is_err());
    }

    #[test]
    fn intent_id_charset_enforced() {
        let mut p = good_plan();
        p.intent_id = "has space".into();
        assert!(p.validate().is_err());
        p.intent_id = "ok-id_1.2".into();
        p.validate().unwrap();
    }

    #[test]
    fn legs_sum_capped_by_total_spend() {
        let mut p = good_plan();
        p.legs = vec![binary_leg(30.0), binary_leg(25.0)];
        // 30 + 25 = 55 > 50 cap
        assert!(p.validate().is_err());
        p.max_total_spend_usdc = 60.0;
        p.validate().unwrap();
    }

    #[test]
    fn range_requires_lower_lt_upper() {
        let mut p = good_plan();
        p.legs = vec![range_leg(84_000.0, 80_000.0)];
        assert!(p.validate().is_err());
    }

    #[test]
    fn max_cost_must_cover_deposit() {
        let mut p = good_plan();
        p.legs = vec![Leg::MintBinary {
            oracle_id: "0xabc".into(),
            strike: 82_000.0,
            side: Side::Up,
            quantity: 50.0,
            deposit: 35.0,
            max_cost: Some(20.0),
            rolling: RollingPolicy::None,
        }];
        assert!(p.validate().is_err());
    }

    #[test]
    fn plan_round_trips_through_json() {
        let p = good_plan();
        let s = serde_json::to_string(&p).unwrap();
        let q: Plan = serde_json::from_str(&s).unwrap();
        q.validate().unwrap();
        assert_eq!(p.intent_id, q.intent_id);
        assert_eq!(p.legs.len(), q.legs.len());
    }
}
