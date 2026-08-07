//! Model-judgment evals (run manually, never in CI): does the REAL model,
//! behind the real gate code and the SHIPPED `config/gate.toml` prompt,
//! make the decisions we want? Scenarios are curated Signal sets with
//! expected outcomes; scoring is deterministic first (decision, P0-5,
//! secret-canary absence), with an optional LLM judge for work-order
//! quality.
//!
//! The model backend is the Claude Code CLI (`claude -p`), so evals run on
//! the operator's existing CLI auth — the same BYO posture as the runner.
//!
//! Everything in this library is deterministic and unit-tested; model
//! calls happen only in the `gate-eval` binary.

pub mod cli_model;
pub mod scenario;
pub mod scoring;

pub use cli_model::CliModel;
pub use scenario::{load_scenarios, Scenario};
pub use scoring::{score, summarize, ScenarioResult, Summary};
