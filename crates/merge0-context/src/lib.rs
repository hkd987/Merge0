//! Context store — a thin assembly layer, not a warehouse (PRD §3).
//!
//! Four context types feed triage; storage lands with the Postgres layer in
//! a follow-up branch. The module boundaries exist now so the Phase 0 work
//! has stable homes.

/// Intent context: per-repo docs pack (MERGE0.md, invariants, feature notes),
/// customer-authored, fetched from git at run time — never stored here.
pub mod intent {}

/// Correlation context: cross-source joins over `Signal::join_keys`
/// (Postgres signal table + join queries).
pub mod correlation {}

/// Release context: deploy timeline and changelog from GitHub Releases +
/// deploy webhooks (`releases` table). Drives first-bad-release attribution
/// (PRD P0-4).
pub mod release {}

/// Outcome memory: inbox verdicts and PR fates (merged / closed / reverted),
/// including revert-as-hard-negative (PRD P0-8). The compounding moat — it
/// only accumulates from running the loop.
pub mod outcome {}
