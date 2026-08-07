//! Triage: scouts + clustering + gate (PRD §4).
//!
//! Division of labor, v1:
//!
//! - **Scouts** ([`scouts`]) are deterministic candidate selectors driven by
//!   their config (sources + schedule window). Their prompts ride along into
//!   gate context; a model-driven scout runtime is a config-compatible
//!   evolution, not a rewrite.
//! - **Clustering** ([`cluster`]) is deterministic: signals correlate by
//!   `join_keys.stack_hash`, then `url_path`, then fingerprint. That is what
//!   makes P0-3 ("same defect from two sources → exactly one Report")
//!   testable without a model in the loop.
//! - **The gate** ([`gate`]) is the model step and the quality bar:
//!   Work Order or SKIP with reason, never silence. Deterministic guards run
//!   before any model call, and P0-5 ("no Work Order without testable
//!   success criteria") is enforced in code, not prompt discipline.
//!
//! Scout and gate prompts are **config files** (`config/`), versioned in git
//! from day one so the meta-loop (PRD §5d) can propose changes as ordinary
//! PRs.

pub mod cluster;
pub mod config;
pub mod gate;
pub mod pipeline;
pub mod scouts;

#[derive(Debug, thiserror::Error)]
pub enum TriageError {
    #[error(transparent)]
    Store(#[from] merge0_store::StoreError),
    #[error(transparent)]
    Model(#[from] merge0_model::ModelError),
    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, TriageError>;

/// Truncate to a character budget, appending a marker — over-budget content
/// is truncated with deep links intact, never silently dropped (PRD §4,
/// evidence budgets).
pub fn truncate_with_marker(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}… [truncated by evidence budget; follow evidence links for the rest]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_marked_never_silent() {
        assert_eq!(truncate_with_marker("short", 100), "short");
        let long = "x".repeat(300);
        let truncated = truncate_with_marker(&long, 100);
        assert!(truncated.starts_with(&"x".repeat(100)));
        assert!(truncated.contains("truncated by evidence budget"));
    }
}
