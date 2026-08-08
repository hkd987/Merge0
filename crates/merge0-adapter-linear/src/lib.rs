//! Linear → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `issues` — a page of the Linear GraphQL `issues` query
//!   (`{"nodes": [...]}`), one `ticket` Signal per issue.
//!
//! Envelope context: `{}` — Linear issues carry their own `url`, so no base
//! URL is needed for deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity** maps from the numeric Linear `priority`: 1 (Urgent) →
//!   critical, 2 (High) → high, 3 (Medium) → medium, 4 (Low) → low. Linear
//!   uses 0 to mean "no priority", so 0 — like an absent or unrecognized
//!   value — also maps to low: an unprioritized issue has not demonstrated
//!   urgency yet.
//! - **Completed and canceled issues are skipped.** An issue whose
//!   `state.type` is `"completed"` or `"canceled"` produces no Signal:
//!   resolved work is not a signal, and re-ingesting a backlog must not
//!   resurrect closed issues. Absent state → not skipped.
//! - **`body`** is the issue `description` (plain markdown); absent/null
//!   description → empty string.
//! - **Delegation.** A label named `merge0` (case-insensitive) marks the
//!   issue as explicitly handed to Merge0: `delegated: true` and severity
//!   floored at high (`severity.max(High)` — an urgent priority stays
//!   critical). Labels arrive as the GraphQL connection shape
//!   (`labels: { nodes: [{ name }] }`, the poller's query) or a flat array
//!   of label objects (`labels: [{ name }]`, the webhook payload shape) —
//!   both are accepted. Absent labels or no matching label →
//!   `delegated: false`, severity unchanged.
//! - **`first_seen`/`last_seen`** come from `createdAt`/`updatedAt` — an
//!   issue "lives" from filing to last activity.
//! - **Evidence** is the issue's own `url` (Linear provides canonical deep
//!   links per issue).
//! - **`join_keys`** are left empty: Linear team/project keys are work
//!   groupings, not org-level account ids.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct LinearAdapter;

#[derive(Debug, Deserialize)]
struct IssuesPage {
    nodes: Vec<serde_json::Value>,
}

/// A Linear issue node — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    identifier: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    /// Numeric: 0 = no priority, 1 = urgent … 4 = low (see module docs).
    #[serde(default)]
    priority: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    url: String,
    #[serde(default)]
    state: Option<State>,
    #[serde(default)]
    labels: Option<Labels>,
}

#[derive(Debug, Deserialize)]
struct State {
    #[serde(default, rename = "type")]
    state_type: Option<String>,
}

/// Issue labels in either shape Linear serves them (see module docs):
/// the GraphQL connection (`{ "nodes": [...] }`) or the webhook's flat
/// array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Labels {
    Connection { nodes: Vec<Label> },
    Flat(Vec<Label>),
}

#[derive(Debug, Deserialize)]
struct Label {
    #[serde(default)]
    name: Option<String>,
}

impl Labels {
    fn contains_merge0(&self) -> bool {
        let nodes = match self {
            Labels::Connection { nodes } => nodes,
            Labels::Flat(nodes) => nodes,
        };
        nodes.iter().any(|label| {
            label
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case("merge0"))
        })
    }
}

impl Adapter for LinearAdapter {
    fn source(&self) -> Source {
        Source::Linear
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "issues" => {
                let page: IssuesPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"nodes\": [...]}}: {e}"))
                    })?;
                let mut signals = Vec::new();
                for node in &page.nodes {
                    if let Some(signal) = normalize_node(node)? {
                        signals.push(signal);
                    }
                }
                Ok(signals)
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one issue node; `Ok(None)` when the issue is skipped (completed
/// or canceled state — see module docs).
fn normalize_node(raw: &serde_json::Value) -> Result<Option<Signal>, AdapterError> {
    let node: IssueNode = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid linear issue: {e}")))?;

    let state_type = node
        .state
        .as_ref()
        .and_then(|s| s.state_type.as_deref())
        .unwrap_or_default();
    if state_type == "completed" || state_type == "canceled" {
        return Ok(None);
    }

    let delegated = node.labels.as_ref().is_some_and(Labels::contains_merge0);
    let mut severity = severity_from_priority(node.priority);
    if delegated {
        // An explicit human delegation is at least high urgency; an urgent
        // priority stays critical.
        severity = severity.max(Severity::High);
    }

    let identifier = node.identifier;
    Ok(Some(Signal {
        id: Ulid::new(),
        source: Source::Linear,
        source_ref: identifier.clone(),
        kind: SignalKind::Ticket,
        severity,
        title: node.title.clone(),
        body: node.description.clone().unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Linear {identifier}"),
            url: node.url.clone(),
        }],
        fingerprint: fingerprint(Source::Linear, &[&identifier]),
        join_keys: JoinKeys::default(),
        affected_count: None,
        delegated,
        first_seen: node.created_at,
        last_seen: node.updated_at,
        raw: raw.clone(),
    }))
}

/// Linear numeric `priority` → severity (see module docs; 0 means "no
/// priority" and maps to low like absent).
fn severity_from_priority(priority: Option<i64>) -> Severity {
    match priority {
        Some(1) => Severity::Critical,
        Some(2) => Severity::High,
        Some(3) => Severity::Medium,
        _ => Severity::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_mapping() {
        assert_eq!(severity_from_priority(Some(1)), Severity::Critical);
        assert_eq!(severity_from_priority(Some(2)), Severity::High);
        assert_eq!(severity_from_priority(Some(3)), Severity::Medium);
        assert_eq!(severity_from_priority(Some(4)), Severity::Low);
        // Linear 0 means "no priority".
        assert_eq!(severity_from_priority(Some(0)), Severity::Low);
        assert_eq!(severity_from_priority(Some(7)), Severity::Low);
        assert_eq!(severity_from_priority(None), Severity::Low);
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": {},
            "payload": payload,
        })
    }

    fn minimal_node(identifier: &str) -> serde_json::Value {
        serde_json::json!({
            "identifier": identifier,
            "title": "Report export fails for large workspaces",
            "url": format!("https://linear.example.com/acme/issue/{identifier}"),
            "createdAt": "2026-08-05T14:00:00Z",
            "updatedAt": "2026-08-06T09:00:00Z"
        })
    }

    fn normalize(nodes: serde_json::Value) -> Vec<Signal> {
        LinearAdapter
            .normalize(&envelope("issues", serde_json::json!({ "nodes": nodes })))
            .unwrap()
    }

    #[test]
    fn completed_and_canceled_states_are_skipped_others_are_not() {
        let mut completed = minimal_node("ENG-1");
        completed["state"] = serde_json::json!({ "name": "Done", "type": "completed" });
        let mut canceled = minimal_node("ENG-2");
        canceled["state"] = serde_json::json!({ "name": "Canceled", "type": "canceled" });
        let mut started = minimal_node("ENG-3");
        started["state"] = serde_json::json!({ "name": "In Progress", "type": "started" });
        // No state at all → not skipped.
        let stateless = minimal_node("ENG-4");

        let signals = normalize(serde_json::json!([completed, canceled, started, stateless]));
        let refs: Vec<&str> = signals.iter().map(|s| s.source_ref.as_str()).collect();
        assert_eq!(refs, ["ENG-3", "ENG-4"]);
    }

    #[test]
    fn merge0_label_delegates_and_floors_severity_case_insensitively() {
        // Mixed-case label among others, GraphQL connection shape →
        // delegated, low → floored to high.
        let mut labeled = minimal_node("ENG-10");
        labeled["labels"] =
            serde_json::json!({ "nodes": [{ "name": "bug" }, { "name": "Merge0" }] });
        let signals = normalize(serde_json::json!([labeled]));
        assert!(signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::High);

        // An urgent priority stays critical when delegated.
        let mut critical = minimal_node("ENG-11");
        critical["labels"] = serde_json::json!({ "nodes": [{ "name": "MERGE0" }] });
        critical["priority"] = serde_json::json!(1);
        let signals = normalize(serde_json::json!([critical]));
        assert!(signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Critical);
    }

    #[test]
    fn webhook_flat_label_array_also_delegates() {
        // Linear webhooks serve labels as a flat array of label objects.
        let mut labeled = minimal_node("ENG-12");
        labeled["labels"] = serde_json::json!([{ "id": "lbl-1", "name": "merge0" }]);
        let signals = normalize(serde_json::json!([labeled]));
        assert!(signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::High);
    }

    #[test]
    fn labels_without_merge0_do_not_delegate() {
        let mut labeled = minimal_node("ENG-13");
        labeled["labels"] = serde_json::json!({ "nodes": [{ "name": "bug" }] });
        let signals = normalize(serde_json::json!([labeled]));
        assert!(!signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Low);
    }

    #[test]
    fn missing_labels_field_does_not_delegate() {
        let signals = normalize(serde_json::json!([minimal_node("ENG-14")]));
        assert!(!signals[0].delegated);
        assert_eq!(signals[0].severity, Severity::Low);
    }

    #[test]
    fn evidence_uses_the_issue_url() {
        let signals = normalize(serde_json::json!([minimal_node("ENG-123")]));
        assert_eq!(
            signals[0].evidence[0].url,
            "https://linear.example.com/acme/issue/ENG-123"
        );
        assert_eq!(signals[0].evidence[0].label, "Linear ENG-123");
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        // Same identifier, different priority/updatedAt → same fingerprint.
        let mut updated = minimal_node("ENG-123");
        updated["priority"] = serde_json::json!(1);
        updated["updatedAt"] = serde_json::json!("2026-08-07T09:00:00Z");
        let a = &normalize(serde_json::json!([minimal_node("ENG-123")]))[0];
        let b = &normalize(serde_json::json!([updated]))[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        // ...while volatile facts still differ.
        assert_ne!(a.severity, b.severity);
    }

    #[test]
    fn null_description_is_empty_body() {
        let mut node = minimal_node("ENG-5");
        node["description"] = serde_json::Value::Null;
        let signals = normalize(serde_json::json!([node]));
        assert_eq!(signals[0].body, "");
    }

    #[test]
    fn malformed_node_is_an_error_not_a_panic() {
        // Missing required `url`.
        let node = serde_json::json!({
            "identifier": "ENG-9",
            "title": "No url on this one",
            "createdAt": "2026-08-05T14:00:00Z",
            "updatedAt": "2026-08-06T09:00:00Z"
        });
        assert!(matches!(
            LinearAdapter.normalize(&envelope("issues", serde_json::json!({ "nodes": [node] }))),
            Err(AdapterError::Malformed(_))
        ));
        // Payload that is not a GraphQL page.
        assert!(matches!(
            LinearAdapter.normalize(&envelope("issues", serde_json::json!({ "wrong": true }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("teams", serde_json::json!({ "nodes": [] }));
        match LinearAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "teams"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }
}
