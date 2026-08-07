//! The meta-loop (PRD §5d): Merge0 on Merge0.
//!
//! Merge0's own operational telemetry (gate precision, dismissal-reason
//! distribution, merge rate) is ingested as just another Signal source
//! ([`MetaAdapter`], `Source::Meta`), and a meta-scout ([`MetaScout`])
//! proposes evidence-linked config-change PRs to the repo where scout/gate
//! prompts live as config files. Same gate, same inbox, same human merge,
//! full rollback — self-tuning with zero new trust machinery, because the
//! improvement loop is the product loop.
//!
//! The §5d invariant governs everything here: **the system improves its
//! artifacts, never itself.** This crate only ever produces Signals and PR
//! proposals against versioned config files; it never mutates prompts,
//! executors, or its own state.

mod adapter;
mod scout;

pub use adapter::MetaAdapter;
pub use scout::{next_severity, open_meta_pr, ConfigProposal, MetaError, MetaScout};
