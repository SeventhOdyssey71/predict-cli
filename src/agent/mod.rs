//! `predict-cli agent`: managed-position layer over the existing trade commands.
//!
//! Design: see `docs/agentic-perps.md`. The agent translates user intent into
//! a validated [`plan::Plan`] of one or more [`plan::Leg`]s and runs it through
//! the existing `commands::trade::*` write paths. Multiple intent surfaces feed
//! the same engine: structured DSL (`agent open`), free-text + regex
//! (`agent ask --provider none`), Anthropic native, and any OpenAI-compatible
//! provider (with tool-using iteration via [`tools`] + [`openai`]).

pub mod exec;
pub mod intent;
pub mod llm;
pub mod openai;
pub mod plan;
pub mod roll;
pub mod store;
pub mod tools;
pub mod watch;
