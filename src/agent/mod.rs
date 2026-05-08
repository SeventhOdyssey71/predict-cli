//! `predict-cli agent`: managed-position layer over the existing trade commands.
//!
//! Design: see `docs/agentic-perps.md`. M1 surface is structured-DSL only
//! (no LLM); a user runs `agent open --side --asset --tenor --risk` and the
//! planner translates that into a [`Plan`] of one or more [`Leg`]s that the
//! executor runs through the existing `commands::trade::*` write paths.

pub mod exec;
pub mod intent;
pub mod plan;
pub mod store;
pub mod watch;
