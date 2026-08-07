//! Reports and gate decisions — the triage half of the pipeline contract.
//!
//! A **Report** is what clustering produces from correlated Signals and what
//! the reviewer sees in the inbox. The **gate** evaluates each maintenance
//! Report into a [`GateDecision`]: a schema-valid [`WorkOrder`](crate::WorkOrder)
//! or a SKIP with a stated reason (PRD §4). Opportunity Reports (PRD P2)
//! never produce a Work Order — their only terminal action is human handoff.

use crate::{EvidenceLink, Severity, WorkOrder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// A defect worth fixing: the standard signals → PR loop.
    Maintenance,
    /// Clustered *demand* evidence (feature requests, drop-offs at missing
    /// affordances, recurring "intended behavior" dismissals). Produces no
    /// Work Order and no PR — terminal action is human handoff.
    Opportunity,
    /// A prevention follow-up proposed by the hardening pass (PRD §5c).
    Hardening,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Assembled by clustering, awaiting the gate.
    Pending,
    /// Gate emitted a Work Order; sitting in the inbox for a human verdict.
    AwaitingReview,
    /// Gate declined (reason recorded on the gate decision).
    Skipped,
    /// Human approved; Work Order queued for dispatch.
    Approved,
    /// Human dismissed (structured reason recorded).
    Dismissed,
    /// Work Order dispatched to customer-side compute.
    Dispatched,
    /// Test-passing PR open, awaiting merge.
    PrOpen,
    /// Terminal: outcome recorded (merged / closed / reverted / discarded).
    Completed,
    /// Terminal for Opportunity Reports: evidence brief handed to a human.
    HandedOff,
}

impl ReportStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ReportStatus::Skipped
                | ReportStatus::Dismissed
                | ReportStatus::Completed
                | ReportStatus::HandedOff
        )
    }
}

/// Structured dismissal reasons (PRD §6) — these feed outcome memory and the
/// false-positive ("intended behavior") trend metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DismissReason {
    IntendedBehavior,
    WontFix,
    Duplicate,
    BadEvidence,
}

impl DismissReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DismissReason::IntendedBehavior => "intended_behavior",
            DismissReason::WontFix => "wont_fix",
            DismissReason::Duplicate => "duplicate",
            DismissReason::BadEvidence => "bad_evidence",
        }
    }
}

/// The gate's output for one Report: Work Order or SKIP, never silence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum GateDecision {
    Work { work_order: WorkOrder },
    Skip { reason: String },
}

/// An evidence-backed report assembled from one or more correlated Signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub id: Ulid,
    pub kind: ReportKind,
    pub title: String,
    /// Assembled narrative: what broke / what users want, since when, impact.
    pub summary: String,
    /// Max severity across member Signals.
    pub severity: Severity,
    /// Evidence assembled under budget (over-budget items become deep links).
    pub evidence: Vec<EvidenceLink>,
    /// Member Signals (at least one).
    pub signal_ids: Vec<Ulid>,
    /// Member fingerprints — the dedupe / recurrence identity of this report.
    pub fingerprints: Vec<String>,
    /// First-bad-release attribution where release context supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspect_release: Option<String>,
    /// Total affected users/accounts across member signals, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_count: Option<u64>,
    pub status: ReportStatus,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_decision_serializes_with_tag() {
        let skip = GateDecision::Skip {
            reason: "no testable success criterion".into(),
        };
        let value = serde_json::to_value(&skip).unwrap();
        assert_eq!(value["decision"], "skip");
        let back: GateDecision = serde_json::from_value(value).unwrap();
        assert_eq!(back, skip);
    }

    #[test]
    fn terminal_statuses() {
        for terminal in [
            ReportStatus::Skipped,
            ReportStatus::Dismissed,
            ReportStatus::Completed,
            ReportStatus::HandedOff,
        ] {
            assert!(terminal.is_terminal());
        }
        for open in [
            ReportStatus::Pending,
            ReportStatus::AwaitingReview,
            ReportStatus::Approved,
            ReportStatus::Dispatched,
            ReportStatus::PrOpen,
        ] {
            assert!(!open.is_terminal());
        }
    }

    #[test]
    fn report_round_trips() {
        let report = Report {
            id: Ulid::new(),
            kind: ReportKind::Maintenance,
            title: "Null district crash in SyncStatusPanel".into(),
            summary: "42 users since v2.3.0, corroborated by rage clicks".into(),
            severity: Severity::High,
            evidence: vec![],
            signal_ids: vec![Ulid::new(), Ulid::new()],
            fingerprints: vec!["sentry:abc".into(), "posthog:def".into()],
            suspect_release: Some("v2.3.0".into()),
            affected_count: Some(42),
            status: ReportStatus::Pending,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }
}
