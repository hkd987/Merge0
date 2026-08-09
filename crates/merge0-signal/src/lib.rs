//! The Signal schema — the contract between every Merge0 component.
//!
//! Normative spec: `docs/signal-schema.md` (v0.6). A test below round-trips
//! the doc's JSON example, so this crate and the doc cannot drift silently.
//! Schema changes must update the doc (and its version) in the same PR.
//!
//! Alongside the Signal itself this crate defines the downstream pipeline
//! contract: [`report::Report`], [`WorkOrder`], gate decisions, outcome
//! records, and telemetry — the types every component exchanges.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

pub mod report;
pub mod telemetry;

pub use report::{DismissReason, GateConfidence, GateDecision, Report, ReportKind, ReportStatus};
pub use telemetry::TelemetrySnapshot;

/// Where a Signal was ingested from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Posthog,
    Sentry,
    Zendesk,
    Intercom,
    Github,
    Webhook,
    Otel,
    Datadog,
    Loopforge,
    Jira,
    Linear,
    /// Messages/threads from designated Slack channels (e.g. #bugs).
    Slack,
    Asana,
    Trello,
    Mixpanel,
    Openpanel,
    /// Merge0's own operational telemetry, ingested as just another source
    /// (the meta-loop, PRD §5d).
    Meta,
}

impl Source {
    /// The stable string form used in serialized Signals and fingerprints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Posthog => "posthog",
            Source::Sentry => "sentry",
            Source::Zendesk => "zendesk",
            Source::Intercom => "intercom",
            Source::Github => "github",
            Source::Webhook => "webhook",
            Source::Otel => "otel",
            Source::Datadog => "datadog",
            Source::Loopforge => "loopforge",
            Source::Jira => "jira",
            Source::Linear => "linear",
            Source::Slack => "slack",
            Source::Asana => "asana",
            Source::Trello => "trello",
            Source::Mixpanel => "mixpanel",
            Source::Openpanel => "openpanel",
            Source::Meta => "meta",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Exception,
    UxFriction,
    Ticket,
    Regression,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Replay,
    StackTrace,
    Ticket,
    Issue,
    Other,
}

/// A deep link back into the source tool, carried through triage into the
/// inbox and the PR description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLink {
    pub kind: EvidenceKind,
    pub label: String,
    pub url: String,
}

/// Correlation context — what lets triage join a Sentry exception with a
/// PostHog rage-click describing the same bug. Adapters must populate every
/// field they can derive; absent fields are omitted from serialization.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JoinKeys {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_path: Option<String>,
}

impl JoinKeys {
    pub fn is_empty(&self) -> bool {
        self.release.is_none()
            && self.stack_hash.is_none()
            && self.account_id.is_none()
            && self.url_path.is_none()
    }
}

/// A normalized signal from any source. See `docs/signal-schema.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub id: Ulid,
    pub source: Source,
    pub source_ref: String,
    pub kind: SignalKind,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub evidence: Vec<EvidenceLink>,
    pub fingerprint: String,
    pub join_keys: JoinKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_count: Option<u64>,
    /// Explicitly handed to Merge0 (e.g. a `merge0` label on the source
    /// ticket). Prioritized by triage; bypasses no safety checks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delegated: bool,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub raw: serde_json::Value,
}

/// Marks an artifact Merge0 itself created in an external tool (today: a
/// tracker story emitted by the delivery layer).
///
/// This is a cross-component contract, not a schema field: the delivery
/// side stamps it, and every adapter that ingests the same tool MUST skip
/// items carrying it. Without that pairing Merge0 re-ingests its own
/// output and triages itself in a loop. Adapter isolation means the writer
/// and the reader cannot depend on each other, so the constant lives here,
/// in the crate both already share.
///
/// It is deliberately distinct from the `merge0` delegation label, which
/// adapters match exactly — `merge0-generated` never means "a human handed
/// this to Merge0".
pub const ORIGIN_LABEL: &str = "merge0-generated";

/// Compute a stable dedupe fingerprint: `<source>:<16-byte-sha256-hex>`.
///
/// The same underlying defect must map to the same `parts` across payload
/// variants and re-ingestion — pass vendor-stable identifiers (issue IDs,
/// normalized culprit frames), never volatile data like counts or timestamps.
pub fn fingerprint(source: Source, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_str().as_bytes());
    for part in parts {
        // Length-prefix each part so ["ab","c"] != ["a","bc"].
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{}", source.as_str(), hex)
}

/// Hash a normalized stack location into a `join_keys.stack_hash` value,
/// comparable across sources (32 hex chars, no source prefix).
pub fn stack_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The gate's output for an actionable Report (PRD §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkOrder {
    pub report_id: Ulid,
    pub repo: String,
    pub summary: String,
    pub evidence: Vec<EvidenceLink>,
    pub repro: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspect_change: Option<String>,
    pub success_criteria: String,
    pub constraints: String,
    pub prior_attempts: Vec<OutcomeRef>,
    /// Maximum change footprint (PRD §5): a run whose fix exceeds this
    /// discards itself with a "fix larger than expected" outcome.
    #[serde(default)]
    pub diff_budget: DiffBudget,
    /// The gate's self-assessed fix confidence — drives the (off-by-default)
    /// auto-dispatch autonomy dial. Absent in old payloads → `Low`.
    #[serde(default)]
    pub confidence: report::GateConfidence,
}

/// The empirical two-regime finding: small scoped PRs merge, sprawling ones
/// die in review. Defaults are the Phase 0 starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffBudget {
    pub max_files: u32,
    pub max_total_lines: u32,
}

impl Default for DiffBudget {
    fn default() -> Self {
        DiffBudget {
            max_files: 4,
            max_total_lines: 150,
        }
    }
}

impl DiffBudget {
    /// Is an observed diff within budget?
    pub fn allows(&self, files_changed: u32, total_lines: u32) -> bool {
        files_changed <= self.max_files && total_lines <= self.max_total_lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Merged,
    Closed,
    Reverted,
    Discarded,
}

/// A pointer into outcome memory: what happened last time this was attempted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRef {
    pub work_order_id: Ulid,
    pub outcome: OutcomeKind,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The PR the attempt produced, when there was one. Schema v0.5: without
    /// it the gate knows only *that* a prior fix was reverted, which can
    /// justify declining but never justifies a better attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON example from `docs/signal-schema.md`, verbatim.
    fn doc_example() -> &'static str {
        let doc = include_str!("../../../docs/signal-schema.md");
        let start = doc
            .find("```json\n")
            .expect("signal-schema.md must contain a ```json example block")
            + "```json\n".len();
        let end = doc[start..]
            .find("\n```")
            .expect("unterminated json block in signal-schema.md")
            + start;
        &doc[start..end]
    }

    #[test]
    fn doc_example_round_trips() {
        let json = doc_example();
        let signal: Signal = serde_json::from_str(json).expect("doc example must deserialize");

        assert_eq!(signal.source, Source::Sentry);
        assert_eq!(signal.kind, SignalKind::Exception);
        assert_eq!(signal.severity, Severity::High);
        assert_eq!(signal.affected_count, Some(42));
        // v0.4: absent `delegated` deserializes false and stays omitted on
        // serialize (the example deliberately leaves it out).
        assert!(!signal.delegated);
        assert_eq!(signal.join_keys.release.as_deref(), Some("v2.3.0"));
        assert!(signal.join_keys.account_id.is_none());
        assert_eq!(signal.evidence.len(), 2);
        assert_eq!(signal.evidence[0].kind, EvidenceKind::Issue);

        // Serialize back and compare as Values: the doc's serialized form is
        // exactly what this crate produces (omitted optionals stay omitted).
        let reserialized = serde_json::to_value(&signal).unwrap();
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(reserialized, original);
    }

    #[test]
    fn fingerprint_is_stable_and_source_prefixed() {
        let a = fingerprint(Source::Sentry, &["5312345678"]);
        let b = fingerprint(Source::Sentry, &["5312345678"]);
        assert_eq!(a, b);
        assert!(a.starts_with("sentry:"));
        // source prefix + ':' + 32 hex chars
        assert_eq!(a.len(), "sentry:".len() + 32);
    }

    #[test]
    fn fingerprint_distinguishes_sources_and_parts() {
        let same_id_other_source = fingerprint(Source::Posthog, &["5312345678"]);
        let sentry = fingerprint(Source::Sentry, &["5312345678"]);
        assert_ne!(same_id_other_source, sentry);

        // Length-prefixing prevents concatenation collisions.
        assert_ne!(
            fingerprint(Source::Sentry, &["ab", "c"]),
            fingerprint(Source::Sentry, &["a", "bc"])
        );
    }

    #[test]
    fn join_keys_serialization_omits_absent_fields() {
        let keys = JoinKeys {
            release: Some("v1.0.0".into()),
            ..Default::default()
        };
        let value = serde_json::to_value(&keys).unwrap();
        assert_eq!(value, serde_json::json!({ "release": "v1.0.0" }));
        assert!(!keys.is_empty());
        assert!(JoinKeys::default().is_empty());
    }

    #[test]
    fn severity_orders_low_to_critical() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn work_order_round_trips() {
        let order = WorkOrder {
            report_id: Ulid::new(),
            repo: "chalk/chalk".into(),
            summary: "Fix null district crash in SyncStatusPanel".into(),
            evidence: vec![],
            repro: "Open /districts/sync for a school with no linked district".into(),
            suspect_change: Some("regressed in v2.3.0".into()),
            success_criteria: "Panel renders empty state; regression test passes".into(),
            constraints: "Do not change sync scheduling logic".into(),
            prior_attempts: vec![OutcomeRef {
                work_order_id: Ulid::new(),
                outcome: OutcomeKind::Reverted,
                occurred_at: Utc::now(),
                note: Some("March attempt reverted: broke district admin view".into()),
                pr_url: Some("https://github.com/chalk/chalk/pull/412".into()),
            }],
            diff_budget: DiffBudget::default(),
            confidence: Default::default(),
        };
        let json = serde_json::to_string(&order).unwrap();
        let back: WorkOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(order, back);
    }

    #[test]
    fn diff_budget_boundaries() {
        let budget = DiffBudget::default();
        assert!(budget.allows(4, 150));
        assert!(!budget.allows(5, 10));
        assert!(!budget.allows(1, 151));
    }
}
