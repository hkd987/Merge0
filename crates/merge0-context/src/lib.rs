//! Context store — a thin assembly layer, not a warehouse (PRD §3).
//!
//! Four context types feed triage:
//!
//! - **Intent** ([`intent`]): the customer-authored MERGE0.md docs pack,
//!   with the machine-managed fenced section the hardening pass may write
//!   (fence rule enforced in code, PRD §5c).
//! - **Correlation**: `join_keys` on Signals — lives in the store's queries.
//! - **Release** ([`release`]): deploy timeline → first-bad-release
//!   attribution (PRD P0-4).
//! - **Outcome memory** ([`outcome`]): verdicts and PR fates by fingerprint
//!   (PRD P0-8), assembled into `WorkOrder::prior_attempts`.

pub mod intent;
pub mod outcome;
pub mod release;
