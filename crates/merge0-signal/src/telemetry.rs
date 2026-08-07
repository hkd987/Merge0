//! Acceptance-rate telemetry (PRD P0-10): the product metric and the sales
//! asset, computed continuously from the first PR.
//!
//! The rate computations live here as pure functions over counts so the
//! store's SQL stays trivial and the math is unit-testable.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Raw counts over a rolling window; the input to [`TelemetrySnapshot`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetryCounts {
    pub window_days: u32,
    /// Work Orders dispatched to customer-side compute.
    pub dispatched: u64,
    /// Test-passing PRs opened.
    pub prs_opened: u64,
    pub prs_merged: u64,
    pub prs_closed: u64,
    pub prs_reverted: u64,
    /// Runs that self-discarded (repair budget exhausted / diff budget hit).
    pub runs_discarded: u64,
    /// Reports approved by a reviewer.
    pub reports_approved: u64,
    /// Reports dismissed, by structured reason.
    pub dismissals: BTreeMap<String, u64>,
    /// Median seconds from PR open to terminal PR outcome.
    pub median_time_to_review_secs: Option<i64>,
    /// Total tokens spent on runs whose PR merged.
    pub tokens_on_merged: Option<u64>,
}

/// Computed rates + the Phase 0 gate check (≥60% merge rate over the window
/// with ≥10 decided PRs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub counts: TelemetryCounts,
    /// merged / (merged + closed + reverted), over PRs that reached a
    /// terminal outcome in the window. None until at least one PR decided.
    pub merge_rate: Option<f64>,
    /// PRs opened / Work Orders dispatched (PRD leading indicator: ≥50%).
    pub runner_yield: Option<f64>,
    /// approved / (approved + dismissed) (PRD leading indicator: ≥70%).
    pub gate_precision: Option<f64>,
    /// Mean tokens per merged PR (P2 cost accounting).
    pub tokens_per_merged_pr: Option<f64>,
    /// The blocking Phase 0 validation gate.
    pub phase0_gate_met: bool,
}

impl TelemetrySnapshot {
    pub fn from_counts(counts: TelemetryCounts) -> Self {
        let decided = counts.prs_merged + counts.prs_closed + counts.prs_reverted;
        let merge_rate = ratio(counts.prs_merged, decided);
        let runner_yield = ratio(counts.prs_opened, counts.dispatched);
        let dismissed: u64 = counts.dismissals.values().sum();
        let gate_precision = ratio(counts.reports_approved, counts.reports_approved + dismissed);
        let tokens_per_merged_pr = match (counts.tokens_on_merged, counts.prs_merged) {
            (Some(tokens), merged) if merged > 0 => Some(tokens as f64 / merged as f64),
            _ => None,
        };
        let phase0_gate_met = decided >= 10 && merge_rate.is_some_and(|r| r >= 0.60);
        TelemetrySnapshot {
            counts,
            merge_rate,
            runner_yield,
            gate_precision,
            tokens_per_merged_pr,
            phase0_gate_met,
        }
    }
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(merged: u64, closed: u64, reverted: u64) -> TelemetryCounts {
        TelemetryCounts {
            window_days: 30,
            prs_merged: merged,
            prs_closed: closed,
            prs_reverted: reverted,
            ..Default::default()
        }
    }

    #[test]
    fn empty_window_has_no_rates_and_gate_unmet() {
        let snap = TelemetrySnapshot::from_counts(TelemetryCounts::default());
        assert_eq!(snap.merge_rate, None);
        assert_eq!(snap.runner_yield, None);
        assert_eq!(snap.gate_precision, None);
        assert!(!snap.phase0_gate_met);
    }

    #[test]
    fn phase0_gate_requires_ten_decided_prs() {
        // 100% merge rate but only 9 PRs: not met.
        let snap = TelemetrySnapshot::from_counts(counts(9, 0, 0));
        assert_eq!(snap.merge_rate, Some(1.0));
        assert!(!snap.phase0_gate_met);
        // 10 decided at exactly 60%: met.
        let snap = TelemetrySnapshot::from_counts(counts(6, 3, 1));
        assert_eq!(snap.merge_rate, Some(0.6));
        assert!(snap.phase0_gate_met);
        // 10 decided below 60%: not met.
        let snap = TelemetrySnapshot::from_counts(counts(5, 4, 1));
        assert!(!snap.phase0_gate_met);
    }

    #[test]
    fn reverts_count_against_merge_rate() {
        let snap = TelemetrySnapshot::from_counts(counts(6, 0, 4));
        assert_eq!(snap.merge_rate, Some(0.6));
    }

    #[test]
    fn gate_precision_and_yield() {
        let mut c = counts(0, 0, 0);
        c.dispatched = 10;
        c.prs_opened = 6;
        c.reports_approved = 7;
        c.dismissals.insert("intended_behavior".into(), 2);
        c.dismissals.insert("duplicate".into(), 1);
        let snap = TelemetrySnapshot::from_counts(c);
        assert_eq!(snap.runner_yield, Some(0.6));
        assert_eq!(snap.gate_precision, Some(0.7));
    }

    #[test]
    fn cost_accounting_tokens_per_merged_pr() {
        let mut c = counts(4, 0, 0);
        c.tokens_on_merged = Some(400_000);
        let snap = TelemetrySnapshot::from_counts(c);
        assert_eq!(snap.tokens_per_merged_pr, Some(100_000.0));
    }
}
