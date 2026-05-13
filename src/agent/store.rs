//! Local position store under `$XDG_CONFIG_HOME/predict-cli/positions.json`
//! (default `~/.config/predict-cli/positions.json`).
//!
//! Atomic writes via tmpfile + rename so a crash mid-write never leaves a
//! half-serialized file. Concurrent CLI invocations rely on this being a single
//! atomic replace; we don't take a file lock because the M1 surface is one
//! shot per invocation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::plan::Plan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    /// Plan recorded; legs not yet submitted.
    Pending,
    /// Legs submitted; awaiting settlement or user close.
    Open,
    /// User closed via `agent close`. Redeem submitted.
    ClosedByUser,
    /// Settled and redeemed (auto via daemon, or permissionless on the user's behalf).
    Settled,
    /// Plan failed mid-execution; some legs may have submitted, some may not.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegRecord {
    /// Index within the plan's `legs` vector.
    pub leg_index: usize,
    /// Status string for diagnostics ("submitted", "redeemed", "failed: …").
    pub status: String,
    /// Submitted-at if known.
    pub submitted_at: Option<DateTime<Utc>>,
    /// Redeemed-at if known.
    pub redeemed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: String,
    pub plan: Plan,
    pub status: PositionStatus,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Per-leg submission/redemption state, parallel-indexed to `plan.legs`.
    pub legs: Vec<LegRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// Schema version. Bump on breaking changes.
    #[serde(default = "current_version")]
    pub version: u32,
    /// Active and historical positions. Keyed by `Position::id`.
    #[serde(default)]
    pub positions: Vec<Position>,
}

const SCHEMA_VERSION: u32 = 1;

fn current_version() -> u32 {
    SCHEMA_VERSION
}

impl Store {
    /// Resolve the on-disk path for the store. Override with
    /// `PREDICT_CLI_STORE` for tests.
    pub fn path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("PREDICT_CLI_STORE") {
            return Ok(PathBuf::from(p));
        }
        let base = if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").context("$HOME is unset; cannot locate config dir")?;
            PathBuf::from(home).join(".config")
        };
        Ok(base.join("predict-cli").join("positions.json"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(Self::default()),
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| format!("parsing position store at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).context("serializing store")?;

        // Atomic rename: write to a sibling tmpfile then rename over the target.
        let tmp = path.with_extension("json.tmp");
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Position> {
        self.positions.iter().find(|p| p.id == id)
    }

    #[allow(dead_code)]
    pub fn find_mut(&mut self, id: &str) -> Option<&mut Position> {
        self.positions.iter_mut().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, pos: Position) {
        if let Some(slot) = self.positions.iter_mut().find(|p| p.id == pos.id) {
            *slot = pos;
        } else {
            self.positions.push(pos);
        }
    }

    #[allow(dead_code)]
    pub fn active(&self) -> impl Iterator<Item = &Position> {
        self.positions
            .iter()
            .filter(|p| matches!(p.status, PositionStatus::Pending | PositionStatus::Open))
    }

    /// Return positions that already used `id`. Useful for "tag is taken" checks.
    pub fn id_in_use(&self, id: &str) -> bool {
        self.positions.iter().any(|p| p.id == id)
    }
}

impl Position {
    /// Build a fresh `Position` from a validated `Plan`.
    pub fn from_plan(plan: Plan) -> Self {
        let n = plan.legs.len();
        let id = plan.intent_id.clone();
        Self {
            id,
            plan,
            status: PositionStatus::Pending,
            opened_at: Utc::now(),
            closed_at: None,
            legs: (0..n)
                .map(|i| LegRecord {
                    leg_index: i,
                    status: "pending".into(),
                    submitted_at: None,
                    redeemed_at: None,
                })
                .collect(),
        }
    }

    pub fn mark_leg(&mut self, idx: usize, status: impl Into<String>) {
        if let Some(l) = self.legs.get_mut(idx) {
            l.status = status.into();
        }
    }

    pub fn mark_leg_submitted(&mut self, idx: usize) {
        if let Some(l) = self.legs.get_mut(idx) {
            l.status = "submitted".into();
            l.submitted_at = Some(Utc::now());
        }
    }

    pub fn mark_leg_redeemed(&mut self, idx: usize) {
        if let Some(l) = self.legs.get_mut(idx) {
            l.status = "redeemed".into();
            l.redeemed_at = Some(Utc::now());
        }
    }
}

/// Generate a default intent id: 8-hex chars seeded by current time. Stable
/// enough for filtering / reference; not cryptographically random.
pub fn auto_intent_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mix = secs.wrapping_mul(2_654_435_761) ^ u64::from(nanos);
    format!("p-{:08x}", (mix as u32) ^ ((mix >> 32) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{ExitPolicy, Leg, Plan, RollingPolicy, Side};
    use std::sync::Mutex;

    fn sample_plan(id: &str) -> Plan {
        Plan {
            intent_id: id.into(),
            directional_view: "long BTC".into(),
            rationale: "test".into(),
            max_total_spend_usdc: 50.0,
            max_total_tenor_minutes: 60,
            legs: vec![Leg::MintBinary {
                oracle_id: "0xabcdef".into(),
                strike: 82_000.0,
                side: Side::Up,
                quantity: 50.0,
                deposit: 35.0,
                max_cost: Some(40.0),
                rolling: RollingPolicy::None,
            }],
            exit_policy: ExitPolicy::default(),
        }
    }

    /// Tests must not touch the user's real `~/.config` store.
    static GUARD: Mutex<()> = Mutex::new(());
    fn isolated_store() -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let g = GUARD.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("predict-cli-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("positions.json");
        let _ = fs::remove_file(&path);
        (path, g)
    }

    #[test]
    fn empty_load_returns_default() {
        let (path, _g) = isolated_store();
        let s = Store::load_from(&path).unwrap();
        assert!(s.positions.is_empty());
    }

    #[test]
    fn round_trip_save_load() {
        let (path, _g) = isolated_store();
        let mut s = Store::default();
        s.upsert(Position::from_plan(sample_plan("test-1")));
        s.save_to(&path).unwrap();

        let loaded = Store::load_from(&path).unwrap();
        assert_eq!(loaded.positions.len(), 1);
        assert_eq!(loaded.positions[0].id, "test-1");
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut s = Store::default();
        s.upsert(Position::from_plan(sample_plan("dup")));
        s.upsert(Position::from_plan(sample_plan("dup")));
        assert_eq!(s.positions.len(), 1);
    }

    #[test]
    fn auto_intent_id_starts_with_p_dash() {
        let id = auto_intent_id();
        assert!(id.starts_with("p-"));
        assert_eq!(id.len(), 10);
    }

    #[test]
    fn active_excludes_closed() {
        let mut s = Store::default();
        let mut p = Position::from_plan(sample_plan("act"));
        p.status = PositionStatus::Open;
        s.upsert(p);
        let mut p2 = Position::from_plan(sample_plan("done"));
        p2.status = PositionStatus::Settled;
        s.upsert(p2);

        let ids: Vec<_> = s.active().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["act"]);
    }
}
