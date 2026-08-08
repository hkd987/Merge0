//! The runner's callback report and server-side budget enforcement.
//!
//! The customer workflow self-reports; Merge0 re-checks the diff budget on
//! the reported numbers ("enforced invariant, not trust"): a run that
//! reports success while exceeding its Work Order's budget is coerced to
//! `Discarded` with the diagnosis preserved as failed-run salvage.

use merge0_signal::WorkOrder;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Test suite green, PR opened.
    Opened,
    /// Self-discarded (repair budget exhausted or diff budget exceeded).
    Discarded,
    /// Infrastructure failure — workflow crashed, agent never ran.
    Failed,
}

/// What the customer workflow POSTs back to Merge0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub report_id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub discard_reason: Option<String>,
    /// Failed-run salvage: the agent's root-cause investigation (PRD §5).
    #[serde(default)]
    pub diagnosis: Option<String>,
    #[serde(default)]
    pub tokens_spent: Option<u64>,
    #[serde(default)]
    pub files_changed: Option<u32>,
    #[serde(default)]
    pub total_lines_changed: Option<u32>,
    /// MCP/skill attribution (PRD §5b) recorded into outcome memory.
    #[serde(default)]
    pub extensions: Option<serde_json::Value>,
}

/// Server-side enforcement. Returns the (possibly coerced) report.
pub fn enforce_budgets(order: &WorkOrder, mut report: RunReport) -> RunReport {
    if report.status != RunStatus::Opened {
        return report;
    }
    // An opened PR must have a URL; anything else is a failed callback.
    if report.pr_url.as_deref().unwrap_or("").is_empty() {
        report.status = RunStatus::Failed;
        report.discard_reason = Some("runner reported opened without a PR URL".into());
        return report;
    }
    let (Some(files), Some(lines)) = (report.files_changed, report.total_lines_changed) else {
        // Footprint unreported: fail closed — the budget is an invariant,
        // not a suggestion (the shipped workflow template always reports it).
        report.status = RunStatus::Discarded;
        report.discard_reason = Some("runner did not report its diff footprint".into());
        return report;
    };
    if !order.diff_budget.allows(files, lines) {
        report.status = RunStatus::Discarded;
        report.discard_reason = Some(format!(
            "fix larger than expected: {files} files / {lines} lines exceeds budget \
             {}/{} — under-specified report or architectural defect",
            order.diff_budget.max_files, order.diff_budget.max_total_lines
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_signal::DiffBudget;
    use ulid::Ulid;

    fn order() -> WorkOrder {
        WorkOrder {
            report_id: Ulid::new(),
            repo: "o/r".into(),
            summary: "s".into(),
            evidence: vec![],
            repro: "r".into(),
            suspect_change: None,
            success_criteria: "c".into(),
            constraints: String::new(),
            prior_attempts: vec![],
            diff_budget: DiffBudget {
                max_files: 4,
                max_total_lines: 150,
            },
            confidence: Default::default(),
        }
    }

    fn opened(files: u32, lines: u32) -> RunReport {
        RunReport {
            report_id: "01ABC".into(),
            status: RunStatus::Opened,
            pr_url: Some("https://github.com/o/r/pull/1".into()),
            branch: Some("merge0/fix".into()),
            discard_reason: None,
            diagnosis: None,
            tokens_spent: Some(90_000),
            files_changed: Some(files),
            total_lines_changed: Some(lines),
            extensions: None,
        }
    }

    #[test]
    fn within_budget_passes_untouched() {
        let report = enforce_budgets(&order(), opened(3, 120));
        assert_eq!(report.status, RunStatus::Opened);
        assert!(report.discard_reason.is_none());
    }

    #[test]
    fn over_budget_success_is_coerced_to_discard() {
        let report = enforce_budgets(&order(), opened(9, 400));
        assert_eq!(report.status, RunStatus::Discarded);
        let reason = report.discard_reason.unwrap();
        assert!(reason.contains("fix larger than expected"));
        assert!(reason.contains("9 files / 400 lines"));
    }

    #[test]
    fn unreported_footprint_fails_closed() {
        let mut report = opened(0, 0);
        report.files_changed = None;
        report.total_lines_changed = None;
        let report = enforce_budgets(&order(), report);
        assert_eq!(report.status, RunStatus::Discarded);
    }

    #[test]
    fn opened_without_pr_url_is_a_failure() {
        let mut report = opened(1, 10);
        report.pr_url = None;
        let report = enforce_budgets(&order(), report);
        assert_eq!(report.status, RunStatus::Failed);
    }

    #[test]
    fn discards_and_failures_pass_through_with_salvage() {
        let report = RunReport {
            report_id: "01ABC".into(),
            status: RunStatus::Discarded,
            pr_url: None,
            branch: None,
            discard_reason: Some("repair budget exhausted".into()),
            diagnosis: Some("root cause: fixture drift in roster tests".into()),
            tokens_spent: Some(40_000),
            files_changed: None,
            total_lines_changed: None,
            extensions: None,
        };
        let out = enforce_budgets(&order(), report.clone());
        assert_eq!(out, report, "non-opened statuses are not rewritten");
    }
}
