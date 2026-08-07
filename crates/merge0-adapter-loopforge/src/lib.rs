//! Loopforge → Signals.
//!
//! Loopforge is the sibling venture's review-pattern miner; Merge0 owns this
//! contract (architectural insurance: the loop stays insights-agnostic by
//! proving a non-observability source normalizes cleanly).
//!
//! Supported envelope endpoints:
//!
//! - `review_findings` — a Loopforge findings export
//!   (`{"findings": [...]}`), one `custom` Signal per finding.
//!
//! No envelope context is required: findings carry their own `pr_url` for
//! deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity** comes from recurrence: a pattern seen in ≥5 review comments
//!   (`occurrences`) → medium, otherwise (including absent) → low. Review
//!   nits are never high/critical on their own.
//! - **`title`** is `"Review pattern in {file}"` — the file is the stable,
//!   human-scannable locus of the pattern; the reviewer's comment becomes the
//!   `body` verbatim.
//! - **`first_seen`/`last_seen`** are both `created_at`: each finding is
//!   reported once; recurrence is expressed by `occurrences`, not a time
//!   span.
//! - **Optional `line` and `author`** are part of the contract but have no
//!   schema home; they are preserved via `raw` only.
//! - **Evidence** is one `other` link labeled "Originating PR" — the PR where
//!   the pattern was flagged.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct LoopforgeAdapter;

#[derive(Debug, Deserialize)]
struct FindingsPage {
    findings: Vec<serde_json::Value>,
}

/// A Loopforge review finding — only the fields we normalize; everything
/// else (including optional `line`/`author`) is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Finding {
    id: String,
    comment: String,
    file: String,
    pr_url: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    occurrences: Option<u64>,
}

impl Adapter for LoopforgeAdapter {
    fn source(&self) -> Source {
        Source::Loopforge
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "review_findings" => {
                let page: FindingsPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"findings\": [...]}}: {e}"))
                    })?;
                page.findings.iter().map(normalize_finding).collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_finding(raw: &serde_json::Value) -> Result<Signal, AdapterError> {
    let finding: Finding = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid loopforge finding: {e}")))?;

    Ok(Signal {
        id: Ulid::new(),
        source: Source::Loopforge,
        source_ref: finding.id.clone(),
        kind: SignalKind::Custom,
        severity: severity_from_occurrences(finding.occurrences),
        title: format!("Review pattern in {}", finding.file),
        body: finding.comment.clone(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: "Originating PR".into(),
            url: finding.pr_url.clone(),
        }],
        fingerprint: fingerprint(Source::Loopforge, &["finding", &finding.id]),
        join_keys: JoinKeys::default(),
        affected_count: finding.occurrences,
        first_seen: finding.created_at,
        last_seen: finding.created_at,
        raw: raw.clone(),
    })
}

/// Recurrence-based severity (see module docs).
fn severity_from_occurrences(occurrences: Option<u64>) -> Severity {
    match occurrences {
        Some(n) if n >= 5 => Severity::Medium,
        _ => Severity::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_thresholds() {
        assert_eq!(severity_from_occurrences(None), Severity::Low);
        assert_eq!(severity_from_occurrences(Some(1)), Severity::Low);
        assert_eq!(severity_from_occurrences(Some(4)), Severity::Low);
        assert_eq!(severity_from_occurrences(Some(5)), Severity::Medium);
        assert_eq!(severity_from_occurrences(Some(50)), Severity::Medium);
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "endpoint": endpoint, "payload": payload })
    }

    fn minimal_finding() -> serde_json::Value {
        serde_json::json!({
            "id": "lf-2041",
            "comment": "Prefer the shared retry helper over hand-rolled backoff loops.",
            "file": "src/sync/backoff.rs",
            "pr_url": "https://github.com/acme/chalk/pull/121",
            "created_at": "2026-08-05T14:00:00Z"
        })
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        // Same finding id, different occurrence count → same fingerprint.
        let mut recurring = minimal_finding();
        recurring["occurrences"] = serde_json::json!(9);
        let a = &LoopforgeAdapter
            .normalize(&envelope(
                "review_findings",
                serde_json::json!({ "findings": [minimal_finding()] }),
            ))
            .unwrap()[0];
        let b = &LoopforgeAdapter
            .normalize(&envelope(
                "review_findings",
                serde_json::json!({ "findings": [recurring] }),
            ))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        // ...while volatile facts still differ.
        assert_ne!(a.severity, b.severity);
        assert_eq!(b.affected_count, Some(9));
    }

    #[test]
    fn title_and_evidence_shape() {
        let signals = LoopforgeAdapter
            .normalize(&envelope(
                "review_findings",
                serde_json::json!({ "findings": [minimal_finding()] }),
            ))
            .unwrap();
        assert_eq!(signals[0].title, "Review pattern in src/sync/backoff.rs");
        assert_eq!(signals[0].evidence.len(), 1);
        assert_eq!(signals[0].evidence[0].label, "Originating PR");
        assert_eq!(
            signals[0].evidence[0].url,
            "https://github.com/acme/chalk/pull/121"
        );
    }

    #[test]
    fn malformed_finding_is_an_error_not_a_panic() {
        // Missing required `comment`.
        let finding = serde_json::json!({
            "id": "lf-2041",
            "file": "src/sync/backoff.rs",
            "pr_url": "https://github.com/acme/chalk/pull/121",
            "created_at": "2026-08-05T14:00:00Z"
        });
        assert!(matches!(
            LoopforgeAdapter.normalize(&envelope(
                "review_findings",
                serde_json::json!({ "findings": [finding] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn optional_and_unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut finding = minimal_finding();
        finding["line"] = serde_json::json!(42);
        finding["author"] = serde_json::json!("chalk-reviewer");
        finding["some_future_loopforge_field"] = serde_json::json!({ "nested": true });
        let signals = LoopforgeAdapter
            .normalize(&envelope(
                "review_findings",
                serde_json::json!({ "findings": [finding] }),
            ))
            .unwrap();
        assert_eq!(signals[0].raw["line"], 42);
        assert_eq!(signals[0].raw["author"], "chalk-reviewer");
        assert_eq!(
            signals[0].raw["some_future_loopforge_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("patterns", serde_json::json!({ "findings": [] }));
        assert!(matches!(
            LoopforgeAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
