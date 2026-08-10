//! Asana → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `tasks` — the Asana list-tasks API response (`{"data": [...]}`), one
//!   `ticket` Signal per incomplete task.
//!
//! Envelope context: `{}` — nothing is needed; Asana tasks carry their own
//! `permalink_url` deep link.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Completed tasks are skipped** (`completed: true` → no Signal). A
//!   finished task is resolved work, not an actionable signal, and emitting
//!   it would only re-open closed loops downstream.
//! - **Severity is always medium.** Asana tasks carry no readable priority,
//!   but polled projects are an explicit watch list — configuring a project
//!   here is triage-by-convention, so its tasks deserve to reach the gate
//!   (medium is exactly the gate's default floor; the gate still decides).
//! - **Evidence** is the task's `permalink_url` (label `Asana task {gid}`,
//!   kind `ticket`). When `permalink_url` is absent (opt fields depend on the
//!   caller's field selection), the Signal is emitted with **no** evidence
//!   links rather than erroring — the triage gate's no-evidence guard handles
//!   that case downstream.
//! - **`first_seen`/`last_seen`** come from `created_at`/`modified_at` — a
//!   task "lives" from creation to last modification.
//! - **`body`** is the task `notes`; absent notes → empty string.
//! - **`join_keys`**: none — an Asana task carries no release, stack, account,
//!   or URL identity we could derive without inventing fields.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct AsanaAdapter;

#[derive(Debug, Deserialize)]
struct TasksPage {
    data: Vec<serde_json::Value>,
}

/// An Asana task — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Task {
    gid: String,
    name: String,
    #[serde(default)]
    notes: Option<String>,
    created_at: DateTime<Utc>,
    modified_at: DateTime<Utc>,
    #[serde(default)]
    completed: bool,
    #[serde(default)]
    permalink_url: Option<String>,
}

impl Adapter for AsanaAdapter {
    fn source(&self) -> Source {
        Source::Asana
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "tasks" => {
                let page: TasksPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"data\": [...]}}: {e}"))
                    })?;
                let mut signals = Vec::new();
                for task in &page.data {
                    if let Some(signal) = normalize_task(task)? {
                        signals.push(signal);
                    }
                }
                Ok(signals)
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one task; `Ok(None)` means "deliberately skipped" (completed
/// task), which is distinct from `Err` (malformed input).
fn normalize_task(raw: &serde_json::Value) -> Result<Option<Signal>, AdapterError> {
    let task: Task = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid asana task: {e}")))?;
    if task.completed {
        return Ok(None);
    }
    let gid = task.gid;

    // Absent permalink_url → no evidence links (see module docs).
    let evidence = match task.permalink_url {
        Some(url) => vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Asana task {gid}"),
            url,
        }],
        None => Vec::new(),
    };

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Asana,
        source_ref: gid.clone(),
        kind: SignalKind::Ticket,
        // Watched projects are triage-by-convention (see module docs).
        severity: Severity::Medium,
        title: task.name,
        body: task.notes.unwrap_or_default(),
        evidence,
        fingerprint: fingerprint(Source::Asana, &[&gid]),
        join_keys: JoinKeys::default(),
        affected_count: None,
        delegated: false,
        first_seen: task.created_at,
        last_seen: task.modified_at,
        raw: raw.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": {},
            "payload": payload,
        })
    }

    fn minimal_task() -> serde_json::Value {
        serde_json::json!({
            "gid": "1207001112223334",
            "name": "Investigate roster import stall",
            "created_at": "2026-08-05T14:00:00Z",
            "modified_at": "2026-08-06T09:00:00Z",
            "completed": false
        })
    }

    fn normalize(payload: serde_json::Value) -> Vec<Signal> {
        AsanaAdapter.normalize(&envelope("tasks", payload)).unwrap()
    }

    #[test]
    fn severity_is_always_medium() {
        let mut task = minimal_task();
        task["name"] = serde_json::json!("URGENT: everything is on fire");
        let signals = normalize(serde_json::json!({ "data": [task] }));
        assert_eq!(signals[0].severity, Severity::Medium);
    }

    #[test]
    fn completed_tasks_are_skipped() {
        let mut done = minimal_task();
        done["gid"] = serde_json::json!("1207009998887776");
        done["completed"] = serde_json::json!(true);
        let signals = normalize(serde_json::json!({ "data": [done, minimal_task()] }));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "1207001112223334");
    }

    #[test]
    fn missing_permalink_yields_no_evidence_not_an_error() {
        let signals = normalize(serde_json::json!({ "data": [minimal_task()] }));
        assert!(signals[0].evidence.is_empty());
    }

    #[test]
    fn permalink_becomes_ticket_evidence() {
        let mut task = minimal_task();
        task["permalink_url"] =
            serde_json::json!("https://app.asana.com/0/1206000111222333/1207001112223334");
        let signals = normalize(serde_json::json!({ "data": [task] }));
        assert_eq!(signals[0].evidence.len(), 1);
        assert_eq!(signals[0].evidence[0].kind, EvidenceKind::Ticket);
        assert_eq!(signals[0].evidence[0].label, "Asana task 1207001112223334");
        assert_eq!(
            signals[0].evidence[0].url,
            "https://app.asana.com/0/1206000111222333/1207001112223334"
        );
    }

    #[test]
    fn notes_become_body_absent_notes_is_empty() {
        let signals = normalize(serde_json::json!({ "data": [minimal_task()] }));
        assert_eq!(signals[0].body, "");

        let mut task = minimal_task();
        task["notes"] = serde_json::json!("Repro steps: import the fall roster.");
        let signals = normalize(serde_json::json!({ "data": [task] }));
        assert_eq!(signals[0].body, "Repro steps: import the fall roster.");
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        let mut modified = minimal_task();
        modified["modified_at"] = serde_json::json!("2026-08-07T12:00:00Z");
        modified["notes"] = serde_json::json!("now with notes");
        let a = &normalize(serde_json::json!({ "data": [minimal_task()] }))[0];
        let b = &normalize(serde_json::json!({ "data": [modified] }))[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.last_seen, b.last_seen);
    }

    #[test]
    fn malformed_task_is_an_error_not_a_panic() {
        // Missing required `name`.
        let task = serde_json::json!({
            "gid": "1207001112223334",
            "created_at": "2026-08-05T14:00:00Z",
            "modified_at": "2026-08-06T09:00:00Z"
        });
        assert!(matches!(
            AsanaAdapter.normalize(&envelope("tasks", serde_json::json!({ "data": [task] }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn malformed_payload_shape_is_an_error() {
        // Payload is not {"data": [...]}.
        assert!(matches!(
            AsanaAdapter.normalize(&envelope("tasks", serde_json::json!({ "tasks": [] }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let result =
            AsanaAdapter.normalize(&envelope("projects", serde_json::json!({ "data": [] })));
        match result {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "projects"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut task = minimal_task();
        task["some_future_asana_field"] = serde_json::json!({ "nested": true });
        let signals = normalize(serde_json::json!({ "data": [task] }));
        assert_eq!(
            signals[0].raw["some_future_asana_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }
}
