//! The Signal schema — the contract between every Merge0 component.
//!
//! Normative spec: `docs/signal-schema.md` (v0.1). A test below round-trips
//! the doc's JSON example, so this crate and the doc cannot drift silently.
//! Schema changes must update the doc (and its version) in the same PR.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

/// Where a Signal was ingested from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Posthog,
    Sentry,
    Zendesk,
    Github,
    Webhook,
}

impl Source {
    /// The stable string form used in serialized Signals and fingerprints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Posthog => "posthog",
            Source::Sentry => "sentry",
            Source::Zendesk => "zendesk",
            Source::Github => "github",
            Source::Webhook => "webhook",
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
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub raw: serde_json::Value,
}

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
            }],
        };
        let json = serde_json::to_string(&order).unwrap();
        let back: WorkOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(order, back);
    }
}
