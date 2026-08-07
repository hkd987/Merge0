//! Triage: scouts + clustering + gate (PRD §4).
//!
//! Scout and gate prompts are **config files, not code** — versioned in
//! `config/` at the repo root from day one so the future meta-loop (PRD §5d)
//! can propose changes as ordinary evidence-linked PRs with zero
//! re-architecture. This crate currently owns the typed loading of that
//! config; scout execution, clustering, and the gate itself land in a
//! follow-up branch.

pub mod config;
