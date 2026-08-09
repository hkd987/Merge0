//! Datadog → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `events` — the Datadog Events API v2 response
//!   (`{"data": [{"id", "type": "event", "attributes": {...}}]}`), one Signal
//!   per event.
//!
//! Envelope context: `{"app_base_url": "https://app.datadoghq.com"}` — used
//! to build event-explorer deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity and kind** map from `alert_type`: error → high/`exception`,
//!   warning → medium/`custom`, info → low/`custom`. Absent or unrecognized
//!   alert_type → medium/`custom` (an event Datadog couldn't classify still
//!   fired, so it is not defaulted to the floor).
//! - **Fingerprint identity** prefers `monitor_id` — re-triggers of the same
//!   monitor are the same underlying defect — and falls back to the event
//!   title for monitor-less events. One of the two is required.
//! - **`join_keys.release`** comes from the `version:` tag when present
//!   (Datadog unified service tagging).
//! - **`first_seen`/`last_seen`** are both the event `timestamp`: an event
//!   is a point-in-time occurrence.
//! - **`body`** is `attributes.message`; absent message → empty string.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct DatadogAdapter;

/// Typed context for the Datadog envelope.
#[derive(Debug, Deserialize)]
struct Context {
    app_base_url: String,
}

#[derive(Debug, Deserialize)]
struct EventsPage {
    data: Vec<serde_json::Value>,
}

/// A Datadog Events API v2 event — only the fields we normalize; everything
/// else is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Event {
    id: String,
    attributes: Attributes,
}

#[derive(Debug, Deserialize)]
struct Attributes {
    timestamp: DateTime<Utc>,
    title: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    alert_type: Option<String>,
    #[serde(default)]
    monitor_id: Option<u64>,
    #[serde(default)]
    tags: Vec<String>,
}

impl Adapter for DatadogAdapter {
    fn source(&self) -> Source {
        Source::Datadog
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid datadog context: {e}")))?;
        let base_url = context.app_base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "events" => {
                let page: EventsPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"data\": [...]}}: {e}"))
                    })?;
                page.data
                    .iter()
                    .map(|event| normalize_event(event, &base_url))
                    .collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_event(raw: &serde_json::Value, base_url: &str) -> Result<Signal, AdapterError> {
    let event: Event = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid datadog event: {e}")))?;
    let attrs = &event.attributes;

    let (severity, kind) = severity_and_kind(attrs.alert_type.as_deref());

    // Monitor identity beats title identity (module docs).
    let fingerprint = match attrs.monitor_id {
        Some(monitor_id) => fingerprint_for(&["event", &monitor_id.to_string()]),
        None => fingerprint_for(&["event", &attrs.title]),
    };

    let release = attrs
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("version:"))
        .map(str::to_string);

    Ok(Signal {
        id: Ulid::new(),
        source: Source::Datadog,
        source_ref: event.id.clone(),
        kind,
        severity,
        title: attrs.title.clone(),
        body: attrs.message.clone().unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: format!("Datadog event {}", event.id),
            url: format!("{base_url}/event/explorer?event={}", event.id),
        }],
        fingerprint,
        join_keys: JoinKeys {
            release,
            ..Default::default()
        },
        affected_count: None,
        delegated: false,
        first_seen: attrs.timestamp,
        last_seen: attrs.timestamp,
        raw: raw.clone(),
    })
}

fn fingerprint_for(parts: &[&str]) -> String {
    fingerprint(Source::Datadog, parts)
}

/// Datadog `alert_type` → (severity, kind) (see module docs).
fn severity_and_kind(alert_type: Option<&str>) -> (Severity, SignalKind) {
    match alert_type {
        Some("error") => (Severity::High, SignalKind::Exception),
        Some("warning") => (Severity::Medium, SignalKind::Custom),
        Some("info") => (Severity::Low, SignalKind::Custom),
        _ => (Severity::Medium, SignalKind::Custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_type_mapping() {
        assert_eq!(
            severity_and_kind(Some("error")),
            (Severity::High, SignalKind::Exception)
        );
        assert_eq!(
            severity_and_kind(Some("warning")),
            (Severity::Medium, SignalKind::Custom)
        );
        assert_eq!(
            severity_and_kind(Some("info")),
            (Severity::Low, SignalKind::Custom)
        );
        assert_eq!(
            severity_and_kind(Some("snapshot")),
            (Severity::Medium, SignalKind::Custom)
        );
        assert_eq!(
            severity_and_kind(None),
            (Severity::Medium, SignalKind::Custom)
        );
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "app_base_url": "https://app.datadoghq.com/" },
            "payload": payload,
        })
    }

    fn monitor_event(id: &str, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "event",
            "attributes": {
                "timestamp": timestamp,
                "title": "[Triggered] High error rate on chalk-api",
                "alert_type": "error",
                "monitor_id": 7654321,
                "tags": ["env:prod", "service:chalk-api", "version:v2.3.0"]
            }
        })
    }

    #[test]
    fn fingerprint_prefers_monitor_id_and_is_stable_across_retriggers() {
        // Same monitor, different event id/timestamp → same fingerprint.
        let a = &DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [monitor_event("AQAAAe-1", "2026-08-05T14:00:00Z")] }),
            ))
            .unwrap()[0];
        let b = &DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [monitor_event("AQAAAe-2", "2026-08-06T09:00:00Z")] }),
            ))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.source_ref, b.source_ref);

        // Without monitor_id the title is the identity.
        let title_only = serde_json::json!({
            "id": "AQAAAe-3",
            "type": "event",
            "attributes": {
                "timestamp": "2026-08-05T14:00:00Z",
                "title": "Deployment finished"
            }
        });
        let c = &DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [title_only] }),
            ))
            .unwrap()[0];
        assert_ne!(a.fingerprint, c.fingerprint);
        assert_eq!(
            c.fingerprint,
            fingerprint(Source::Datadog, &["event", "Deployment finished"])
        );
    }

    #[test]
    fn release_comes_from_version_tag() {
        let signals = DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [monitor_event("AQAAAe-1", "2026-08-05T14:00:00Z")] }),
            ))
            .unwrap();
        assert_eq!(signals[0].join_keys.release.as_deref(), Some("v2.3.0"));

        let mut no_version = monitor_event("AQAAAe-1", "2026-08-05T14:00:00Z");
        no_version["attributes"]["tags"] = serde_json::json!(["env:prod"]);
        let signals = DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [no_version] }),
            ))
            .unwrap();
        assert_eq!(signals[0].join_keys.release, None);
    }

    #[test]
    fn evidence_deep_link_uses_app_base_url() {
        let signals = DatadogAdapter
            .normalize(&envelope(
                "events",
                serde_json::json!({ "data": [monitor_event("AQAAAe-1", "2026-08-05T14:00:00Z")] }),
            ))
            .unwrap();
        assert_eq!(
            signals[0].evidence[0].url,
            "https://app.datadoghq.com/event/explorer?event=AQAAAe-1"
        );
    }

    #[test]
    fn malformed_event_is_an_error_not_a_panic() {
        // Missing required `attributes.timestamp`.
        let event = serde_json::json!({
            "id": "AQAAAe-1",
            "type": "event",
            "attributes": { "title": "Broken" }
        });
        assert!(matches!(
            DatadogAdapter.normalize(&envelope("events", serde_json::json!({ "data": [event] }))),
            Err(AdapterError::Malformed(_))
        ));

        // Missing required `attributes.title`.
        let event = serde_json::json!({
            "id": "AQAAAe-1",
            "type": "event",
            "attributes": { "timestamp": "2026-08-05T14:00:00Z" }
        });
        assert!(matches!(
            DatadogAdapter.normalize(&envelope("events", serde_json::json!({ "data": [event] }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut event = monitor_event("AQAAAe-1", "2026-08-05T14:00:00Z");
        event["attributes"]["some_future_datadog_field"] = serde_json::json!({ "nested": 1 });
        let signals = DatadogAdapter
            .normalize(&envelope("events", serde_json::json!({ "data": [event] })))
            .unwrap();
        assert_eq!(
            signals[0].raw["attributes"]["some_future_datadog_field"]["nested"],
            1
        );
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("monitors", serde_json::json!({ "data": [] }));
        assert!(matches!(
            DatadogAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
