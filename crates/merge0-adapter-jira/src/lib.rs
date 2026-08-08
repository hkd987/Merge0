//! Jira → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `issues` — a Jira Cloud search response (`{"issues": [...]}`), one
//!   `ticket` Signal per issue.
//!
//! Envelope context:
//! `{"browse_base_url": "https://acme-example.atlassian.net/browse"}` — used
//! to build issue deep links (`{browse_base_url}/{key}`).
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity** maps from `fields.priority.name`: Highest → critical,
//!   High → high, Medium → medium, Low/Lowest → low. Absent (priority is
//!   optional on Jira issues) or unrecognized priority → low: an untriaged
//!   issue has not demonstrated urgency yet.
//! - **Done issues are skipped.** An issue whose
//!   `fields.status.statusCategory.key` is `"done"` produces no Signal:
//!   resolved work is not a signal, and re-ingesting a backlog must not
//!   resurrect closed issues. Absent status → not skipped.
//! - **Anti-loop.** Merge0 files stories into Jira as well as reading from
//!   it, so an issue labelled `merge0_signal::ORIGIN_LABEL` is Merge0's own
//!   output and produces no Signal. Without that, the loop re-ingests
//!   itself forever. The check runs *before* delegation, so a story we
//!   wrote can never delegate work back to us whatever else it is labelled.
//! - **`body`** comes from `fields.description`, which Jira serves in two
//!   shapes: a plain string (API v2) is used verbatim; an Atlassian Document
//!   Format object (API v3) is flattened by concatenating every `"text"`
//!   field found recursively, in document order. Absent description or any
//!   other/unknown shape → empty body, never an error.
//! - **Delegation.** A `fields.labels` entry equal to `merge0`
//!   (case-insensitive) marks the ticket as explicitly handed to Merge0:
//!   `delegated: true` and severity floored at high
//!   (`severity.max(High)` — a critical priority stays critical). Absent
//!   `labels` or no matching label → `delegated: false`, severity unchanged.
//! - **`first_seen`/`last_seen`** come from `fields.created`/`fields.updated`
//!   — an issue "lives" from filing to last activity.
//! - **`join_keys`** are left empty: `fields.project.key` is a repo/project
//!   grouping, not an org-level account id, so it does not populate
//!   `account_id`.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
    ORIGIN_LABEL,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct JiraAdapter;

/// Typed context for the Jira envelope.
#[derive(Debug, Deserialize)]
struct Context {
    browse_base_url: String,
}

#[derive(Debug, Deserialize)]
struct SearchPage {
    issues: Vec<serde_json::Value>,
}

/// A Jira issue — only the fields we normalize; everything else is preserved
/// via `raw`.
#[derive(Debug, Deserialize)]
struct Issue {
    key: String,
    fields: Fields,
}

#[derive(Debug, Deserialize)]
struct Fields {
    summary: String,
    /// Plain string (API v2) or ADF object (API v3); see module docs.
    #[serde(default)]
    description: Option<serde_json::Value>,
    #[serde(default)]
    priority: Option<Priority>,
    created: DateTime<Utc>,
    updated: DateTime<Utc>,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Priority {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default, rename = "statusCategory")]
    status_category: Option<StatusCategory>,
}

#[derive(Debug, Deserialize)]
struct StatusCategory {
    #[serde(default)]
    key: Option<String>,
}

impl Adapter for JiraAdapter {
    fn source(&self) -> Source {
        Source::Jira
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid jira context: {e}")))?;
        let base_url = context.browse_base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "issues" => {
                let page: SearchPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"issues\": [...]}}: {e}"))
                    })?;
                let mut signals = Vec::new();
                for issue in &page.issues {
                    if let Some(signal) = normalize_issue(issue, &base_url)? {
                        signals.push(signal);
                    }
                }
                Ok(signals)
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one issue; `Ok(None)` when the issue is skipped (done status
/// category — see module docs).
fn normalize_issue(
    raw: &serde_json::Value,
    base_url: &str,
) -> Result<Option<Signal>, AdapterError> {
    let issue: Issue = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid jira issue: {e}")))?;

    let status_category = issue
        .fields
        .status
        .as_ref()
        .and_then(|s| s.status_category.as_ref())
        .and_then(|c| c.key.as_deref());
    if status_category == Some("done") {
        return Ok(None);
    }

    // Anti-loop: Merge0 both files stories into Jira and ingests from it.
    // Without this, every story it writes comes straight back as a signal
    // and the system triages its own output forever. Checked BEFORE
    // delegation so the skip is structural — our own story can never
    // delegate work to us, whatever labels it also carries.
    if issue
        .fields
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(ORIGIN_LABEL))
    {
        return Ok(None);
    }

    let delegated = issue
        .fields
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("merge0"));
    let mut severity = severity_from_priority(
        issue
            .fields
            .priority
            .as_ref()
            .and_then(|p| p.name.as_deref()),
    );
    if delegated {
        // An explicit human delegation is at least high urgency; a critical
        // priority stays critical.
        severity = severity.max(Severity::High);
    }

    let key = issue.key;
    Ok(Some(Signal {
        id: Ulid::new(),
        source: Source::Jira,
        source_ref: key.clone(),
        kind: SignalKind::Ticket,
        severity,
        title: issue.fields.summary.clone(),
        body: issue
            .fields
            .description
            .as_ref()
            .map(description_text)
            .unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Jira {key}"),
            url: format!("{base_url}/{key}"),
        }],
        fingerprint: fingerprint(Source::Jira, &[&key]),
        join_keys: JoinKeys::default(),
        affected_count: None,
        delegated,
        first_seen: issue.fields.created,
        last_seen: issue.fields.updated,
        raw: raw.clone(),
    }))
}

/// Jira `priority.name` → severity (see module docs).
fn severity_from_priority(priority: Option<&str>) -> Severity {
    match priority {
        Some("Highest") => Severity::Critical,
        Some("High") => Severity::High,
        Some("Medium") => Severity::Medium,
        _ => Severity::Low,
    }
}

/// `fields.description` → body text: strings verbatim, ADF objects flattened
/// via [`adf_text`], anything else (unknown shape) → empty string. Never an
/// error — a description we cannot read should not block the signal.
fn description_text(description: &serde_json::Value) -> String {
    match description {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(_) => {
            let mut out = String::new();
            adf_text(description, &mut out);
            out
        }
        _ => String::new(),
    }
}

/// Concatenate every `"text"` field found recursively in an Atlassian
/// Document Format value, in document order (a node's own text before its
/// children's).
fn adf_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(text)) = map.get("text") {
                out.push_str(text);
            }
            for (key, child) in map {
                if key != "text" {
                    adf_text(child, out);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                adf_text(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_mapping() {
        assert_eq!(severity_from_priority(Some("Highest")), Severity::Critical);
        assert_eq!(severity_from_priority(Some("High")), Severity::High);
        assert_eq!(severity_from_priority(Some("Medium")), Severity::Medium);
        assert_eq!(severity_from_priority(Some("Low")), Severity::Low);
        assert_eq!(severity_from_priority(Some("Lowest")), Severity::Low);
        assert_eq!(severity_from_priority(Some("unheard-of")), Severity::Low);
        assert_eq!(severity_from_priority(None), Severity::Low);
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "browse_base_url": "https://acme-example.atlassian.net/browse/" },
            "payload": payload,
        })
    }

    fn minimal_issue(key: &str) -> serde_json::Value {
        serde_json::json!({
            "key": key,
            "fields": {
                "summary": "Checkout spinner never resolves",
                "created": "2026-08-05T14:00:00Z",
                "updated": "2026-08-06T09:00:00Z"
            }
        })
    }

    fn normalize(issues: serde_json::Value) -> Vec<Signal> {
        JiraAdapter
            .normalize(&envelope("issues", serde_json::json!({ "issues": issues })))
            .unwrap()
    }

    #[test]
    fn done_status_category_is_skipped_others_are_not() {
        let mut done = minimal_issue("CHK-1");
        done["fields"]["status"] =
            serde_json::json!({ "name": "Done", "statusCategory": { "key": "done" } });
        let mut in_progress = minimal_issue("CHK-2");
        in_progress["fields"]["status"] = serde_json::json!({
            "name": "In Progress",
            "statusCategory": { "key": "indeterminate" }
        });
        // No status at all → not skipped.
        let statusless = minimal_issue("CHK-3");

        let signals = normalize(serde_json::json!([done, in_progress, statusless]));
        let refs: Vec<&str> = signals.iter().map(|s| s.source_ref.as_str()).collect();
        assert_eq!(refs, ["CHK-2", "CHK-3"]);
    }

    #[test]
    fn adf_description_is_flattened_to_text() {
        let mut issue = minimal_issue("CHK-4");
        issue["fields"]["description"] = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        { "type": "text", "text": "Clicking " },
                        { "type": "text", "text": "Export", "marks": [{ "type": "strong" }] },
                        { "type": "text", "text": " does nothing." }
                    ]
                }
            ]
        });
        let signals = normalize(serde_json::json!([issue]));
        assert_eq!(signals[0].body, "Clicking Export does nothing.");
    }

    #[test]
    fn string_description_is_used_verbatim() {
        let mut issue = minimal_issue("CHK-5");
        issue["fields"]["description"] = serde_json::json!("Plain text description.");
        let signals = normalize(serde_json::json!([issue]));
        assert_eq!(signals[0].body, "Plain text description.");
    }

    #[test]
    fn unknown_description_shape_is_empty_body_not_an_error() {
        for weird in [
            serde_json::json!(42),
            serde_json::json!(["not", "a", "doc"]),
            serde_json::json!(true),
        ] {
            let mut issue = minimal_issue("CHK-6");
            issue["fields"]["description"] = weird;
            let signals = normalize(serde_json::json!([issue]));
            assert_eq!(signals[0].body, "");
        }
        // Absent description too.
        let signals = normalize(serde_json::json!([minimal_issue("CHK-7")]));
        assert_eq!(signals[0].body, "");
    }

    #[test]
    fn merge0_label_delegates_and_floors_severity_case_insensitively() {
        // Mixed-case label among others → delegated, low → floored to high.
        let mut labeled = minimal_issue("CHK-10");
        labeled["fields"]["labels"] = serde_json::json!(["checkout", "Merge0"]);
        let signals = normalize(serde_json::json!([labeled]));
        assert!(signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::High);

        // A critical priority stays critical when delegated.
        let mut critical = minimal_issue("CHK-11");
        critical["fields"]["labels"] = serde_json::json!(["MERGE0"]);
        critical["fields"]["priority"] = serde_json::json!({ "name": "Highest" });
        let signals = normalize(serde_json::json!([critical]));
        assert!(signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Critical);
    }

    #[test]
    fn labels_without_merge0_do_not_delegate() {
        let mut labeled = minimal_issue("CHK-12");
        labeled["fields"]["labels"] = serde_json::json!(["checkout", "payments"]);
        let signals = normalize(serde_json::json!([labeled]));
        assert!(!signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Low);
    }

    #[test]
    fn missing_labels_field_does_not_delegate() {
        let signals = normalize(serde_json::json!([minimal_issue("CHK-13")]));
        assert!(!signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Low);
    }

    #[test]
    fn evidence_deep_link_uses_browse_base_url() {
        let signals = normalize(serde_json::json!([minimal_issue("CHK-42")]));
        assert_eq!(
            signals[0].evidence[0].url,
            "https://acme-example.atlassian.net/browse/CHK-42"
        );
        assert_eq!(signals[0].evidence[0].label, "Jira CHK-42");
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        // Same key, different priority/updated → same fingerprint.
        let mut updated = minimal_issue("CHK-42");
        updated["fields"]["priority"] = serde_json::json!({ "name": "Highest" });
        updated["fields"]["updated"] = serde_json::json!("2026-08-07T09:00:00Z");
        let a = &normalize(serde_json::json!([minimal_issue("CHK-42")]))[0];
        let b = &normalize(serde_json::json!([updated]))[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        // ...while volatile facts still differ.
        assert_ne!(a.severity, b.severity);
    }

    #[test]
    fn malformed_issue_is_an_error_not_a_panic() {
        // Missing required `fields.summary`.
        let issue = serde_json::json!({
            "key": "CHK-9",
            "fields": {
                "created": "2026-08-05T14:00:00Z",
                "updated": "2026-08-06T09:00:00Z"
            }
        });
        assert!(matches!(
            JiraAdapter.normalize(&envelope(
                "issues",
                serde_json::json!({ "issues": [issue] })
            )),
            Err(AdapterError::Malformed(_))
        ));
        // Payload that is not a search page.
        assert!(matches!(
            JiraAdapter.normalize(&envelope("issues", serde_json::json!({ "wrong": true }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("projects", serde_json::json!({ "issues": [] }));
        match JiraAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "projects"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod origin_label_tests {
    use super::*;

    fn envelope(issues: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": "issues",
            "context": { "browse_base_url": "https://acme-example.atlassian.net/browse" },
            "payload": { "issues": issues },
        })
    }

    fn issue(key: &str, labels: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "key": key,
            "fields": {
                "summary": "Something is broken",
                "priority": { "name": "High" },
                "labels": labels,
                "status": { "statusCategory": { "key": "indeterminate" } },
                "created": "2026-08-07T08:00:00.000Z",
                "updated": "2026-08-07T09:00:00.000Z",
            }
        })
    }

    /// A story Merge0 filed must never come back as a signal — case
    /// insensitively — while its ordinary neighbours still do.
    #[test]
    fn merge0_generated_issues_are_skipped_entirely() {
        for label in ["merge0-generated", "Merge0-Generated"] {
            let signals = JiraAdapter
                .normalize(&envelope(serde_json::json!([
                    issue("CHK-1", serde_json::json!([label])),
                    issue("CHK-2", serde_json::json!(["backend"])),
                ])))
                .unwrap();
            let keys: Vec<&str> = signals.iter().map(|s| s.source_ref.as_str()).collect();
            assert_eq!(keys, vec!["CHK-2"], "{label} must be skipped");
        }
    }

    /// Our own story carrying the delegation label too is still ours: the
    /// skip must win, or a story we wrote could delegate work back to us.
    #[test]
    fn the_origin_skip_beats_the_delegation_label() {
        let signals = JiraAdapter
            .normalize(&envelope(serde_json::json!([issue(
                "CHK-3",
                serde_json::json!(["merge0", "merge0-generated"])
            )])))
            .unwrap();
        assert!(signals.is_empty(), "origin skip must beat delegation");
    }
}
