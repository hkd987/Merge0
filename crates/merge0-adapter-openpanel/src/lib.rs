//! OpenPanel → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `events` — the OpenPanel `GET /export/events` response
//!   (`{"meta": {...}, "data": [...]}`), aggregated into one `exception`
//!   Signal per `(name, path)` group.
//!
//! Envelope context:
//! `{"project_base_url": "https://openpanel.example.com/acme/website"}` —
//! used to build event-explorer deep links.
//!
//! The poller filters to operator-configured error-shaped event names before
//! building the envelope, so every event in `data` is already signal-worthy —
//! this adapter aggregates, it does not re-filter.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **`affected_count`** is the distinct non-empty `profileId` count in the
//!   group — distinct humans, not event volume. When no event carries a
//!   profile, the group's event count is the fallback impact measure.
//! - **Severity** comes from that count: ≥100 critical, ≥20 high, ≥5 medium,
//!   else low. Conservative by design — a quiet inbox that's right beats a
//!   busy one.
//! - **`join_keys.url_path`** is the group's `path` when non-empty;
//!   **`join_keys.account_id`** is set only when exactly one distinct
//!   non-empty `profileId` is involved (the same convention the PostHog
//!   adapter uses for rage-click groups).
//! - **A malformed event** (missing `name`, unparseable `createdAt`) is
//!   skipped, not an envelope failure: one corrupt row must not sink the
//!   rest of an already-filtered export page.
//! - **`first_seen`/`last_seen`** are the min/max `createdAt` over the group.
//! - **`body`** stays factual — counts, path, and a representative
//!   `properties.message` when present; never raw JSON.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use ulid::Ulid;

pub struct OpenpanelAdapter;

/// Typed context for the OpenPanel envelope.
#[derive(Debug, Deserialize)]
struct Context {
    project_base_url: String,
}

#[derive(Debug, Deserialize)]
struct ExportPage {
    data: Vec<serde_json::Value>,
}

/// An OpenPanel exported event — only the fields we normalize; everything
/// else is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct ExportedEvent {
    name: String,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "profileId", default)]
    profile_id: Option<String>,
    #[serde(default)]
    path: String,
    /// Free-form event properties; only a string `message` is surfaced, and
    /// tolerantly — shape variance here must not invalidate the event.
    #[serde(default)]
    properties: serde_json::Value,
}

impl Adapter for OpenpanelAdapter {
    fn source(&self) -> Source {
        Source::Openpanel
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid openpanel context: {e}")))?;
        let base_url = context.project_base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "events" => {
                let page: ExportPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"data\": [...]}}: {e}"))
                    })?;
                Ok(normalize_events(&page.data, &base_url))
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_events(events: &[serde_json::Value], base_url: &str) -> Vec<Signal> {
    struct Group {
        raws: Vec<serde_json::Value>,
        profiles: Vec<String>,
        message: Option<String>,
        first_seen: DateTime<Utc>,
        last_seen: DateTime<Utc>,
    }

    // BTreeMap keyed by (name, path) → deterministic output order.
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();

    for raw in events {
        // Malformed events are skipped, not fatal (module docs).
        let Ok(event) = serde_json::from_value::<ExportedEvent>(raw.clone()) else {
            continue;
        };

        let group = groups
            .entry((event.name.clone(), event.path.clone()))
            .or_insert_with(|| Group {
                raws: vec![],
                profiles: vec![],
                message: None,
                first_seen: event.created_at,
                last_seen: event.created_at,
            });
        group.raws.push(raw.clone());
        if let Some(profile) = event.profile_id.as_deref().filter(|p| !p.is_empty()) {
            if !group.profiles.contains(&profile.to_string()) {
                group.profiles.push(profile.to_string());
            }
        }
        if group.message.is_none() {
            group.message = event
                .properties
                .get("message")
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty())
                .map(str::to_string);
        }
        group.first_seen = group.first_seen.min(event.created_at);
        group.last_seen = group.last_seen.max(event.created_at);
    }

    groups
        .into_iter()
        .map(|((name, path), group)| {
            let user_count = group.profiles.len() as u64;
            // Distinct humans when profiles exist; event volume otherwise
            // (module docs).
            let (affected, unit) = if user_count > 0 {
                (user_count, "user")
            } else {
                (group.raws.len() as u64, "event")
            };
            let plural = if affected == 1 { "" } else { "s" };
            let subject = if path.is_empty() {
                name.clone()
            } else {
                format!("{name} on {path}")
            };
            let mut body = format!("{} '{name}' event(s)", group.raws.len());
            if !path.is_empty() {
                body.push_str(&format!(" on {path}"));
            }
            if user_count > 0 {
                body.push_str(&format!(" from {user_count} distinct user(s)"));
            }
            if let Some(message) = &group.message {
                body.push_str(&format!("; example message: {message}"));
            }
            let account_id = if group.profiles.len() == 1 {
                Some(group.profiles[0].clone())
            } else {
                None
            };
            Signal {
                id: Ulid::generate(),
                source: Source::Openpanel,
                source_ref: format!("{name}:{path}"),
                kind: SignalKind::Exception,
                severity: severity_from_impact(affected),
                title: format!("Error event: {subject} ({affected} {unit}{plural})"),
                body,
                evidence: vec![EvidenceLink {
                    kind: EvidenceKind::Issue,
                    label: format!("openpanel events {name}"),
                    url: format!("{base_url}/events?event={name}"),
                }],
                fingerprint: fingerprint(Source::Openpanel, &["event", &name, &path]),
                join_keys: JoinKeys {
                    account_id,
                    url_path: (!path.is_empty()).then(|| path.clone()),
                    ..Default::default()
                },
                affected_count: Some(affected),
                delegated: false,
                first_seen: group.first_seen,
                last_seen: group.last_seen,
                raw: serde_json::Value::Array(group.raws),
            }
        })
        .collect()
}

/// Impact-based severity for event groups (see module docs).
fn severity_from_impact(count: u64) -> Severity {
    match count {
        n if n >= 100 => Severity::Critical,
        n if n >= 20 => Severity::High,
        n if n >= 5 => Severity::Medium,
        _ => Severity::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity_from_impact(1), Severity::Low);
        assert_eq!(severity_from_impact(4), Severity::Low);
        assert_eq!(severity_from_impact(5), Severity::Medium);
        assert_eq!(severity_from_impact(19), Severity::Medium);
        assert_eq!(severity_from_impact(20), Severity::High);
        assert_eq!(severity_from_impact(99), Severity::High);
        assert_eq!(severity_from_impact(100), Severity::Critical);
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "project_base_url": "https://openpanel.example.com/acme/website" },
            "payload": payload,
        })
    }

    fn event(name: &str, path: &str, profile: &str, created_at: &str) -> serde_json::Value {
        serde_json::json!({
            "id": format!("01K3{name}{profile}"),
            "name": name,
            "deviceId": "d-1",
            "profileId": profile,
            "projectId": "website",
            "sessionId": "s-1",
            "properties": {},
            "createdAt": created_at,
            "country": "US",
            "city": "Denver",
            "region": "CO",
            "os": "macOS",
            "osVersion": "14.5",
            "browser": "Chrome",
            "browserVersion": "126",
            "device": "desktop",
            "brand": "",
            "model": "",
            "path": path,
            "origin": "https://app.example.com",
            "referrer": "",
            "referrerName": "",
            "referrerType": ""
        })
    }

    fn payload(events: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "meta": { "count": events.len(), "totalCount": events.len(), "pages": 1, "current": 1 },
            "data": events,
        })
    }

    #[test]
    fn events_group_by_name_and_path() {
        let input = envelope(
            "events",
            payload(vec![
                event(
                    "payment_failed",
                    "/checkout",
                    "p-1",
                    "2026-08-08T10:00:00.000Z",
                ),
                event(
                    "payment_failed",
                    "/checkout",
                    "p-2",
                    "2026-08-08T11:00:00.000Z",
                ),
                event(
                    "payment_failed",
                    "/billing",
                    "p-1",
                    "2026-08-08T12:00:00.000Z",
                ),
                event("api_error", "/checkout", "p-3", "2026-08-08T13:00:00.000Z"),
            ]),
        );
        let signals = OpenpanelAdapter.normalize(&input).unwrap();
        // BTreeMap order: (api_error, /checkout), (payment_failed, /billing),
        // (payment_failed, /checkout).
        assert_eq!(signals.len(), 3);
        assert_eq!(signals[0].source_ref, "api_error:/checkout");
        assert_eq!(signals[1].source_ref, "payment_failed:/billing");
        assert_eq!(signals[2].source_ref, "payment_failed:/checkout");
        assert_eq!(signals[2].affected_count, Some(2));
        assert_eq!(signals[2].kind, SignalKind::Exception);
        // Same event name on different paths → different fingerprints.
        assert_ne!(signals[1].fingerprint, signals[2].fingerprint);
    }

    #[test]
    fn account_id_only_for_a_single_distinct_profile() {
        let input = envelope(
            "events",
            payload(vec![
                // Two events, one profile → account_id set.
                event("form_error", "/signup", "p-9", "2026-08-08T10:00:00.000Z"),
                event("form_error", "/signup", "p-9", "2026-08-08T11:00:00.000Z"),
                // Two profiles → no account_id.
                event("api_error", "/dashboard", "p-1", "2026-08-08T10:00:00.000Z"),
                event("api_error", "/dashboard", "p-2", "2026-08-08T11:00:00.000Z"),
            ]),
        );
        let signals = OpenpanelAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].join_keys.account_id, None);
        assert_eq!(signals[0].join_keys.url_path.as_deref(), Some("/dashboard"));
        assert_eq!(signals[1].join_keys.account_id.as_deref(), Some("p-9"));
        assert_eq!(signals[1].affected_count, Some(1));
        assert_eq!(
            signals[1].title,
            "Error event: form_error on /signup (1 user)"
        );
    }

    #[test]
    fn profileless_groups_fall_back_to_event_count() {
        let input = envelope(
            "events",
            payload(vec![
                event("boot_error", "", "", "2026-08-08T10:00:00.000Z"),
                event("boot_error", "", "", "2026-08-08T11:00:00.000Z"),
            ]),
        );
        let signals = OpenpanelAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(signal.affected_count, Some(2));
        assert_eq!(signal.title, "Error event: boot_error (2 events)");
        assert_eq!(signal.body, "2 'boot_error' event(s)");
        // Empty path/profile → no join keys at all.
        assert!(signal.join_keys.is_empty());
        assert_eq!(signal.source_ref, "boot_error:");
    }

    #[test]
    fn severity_uses_distinct_profiles_not_event_volume() {
        // Six events from two profiles stay low; five profiles reach medium.
        let two_profiles: Vec<_> = (0..6)
            .map(|i| {
                event(
                    "api_error",
                    "/dashboard",
                    if i % 2 == 0 { "p-1" } else { "p-2" },
                    "2026-08-08T10:00:00.000Z",
                )
            })
            .collect();
        let signals = OpenpanelAdapter
            .normalize(&envelope("events", payload(two_profiles)))
            .unwrap();
        assert_eq!(signals[0].severity, Severity::Low);

        let five_profiles: Vec<_> = (0..5)
            .map(|i| {
                event(
                    "api_error",
                    "/dashboard",
                    &format!("p-{i}"),
                    "2026-08-08T10:00:00.000Z",
                )
            })
            .collect();
        let signals = OpenpanelAdapter
            .normalize(&envelope("events", payload(five_profiles)))
            .unwrap();
        assert_eq!(signals[0].severity, Severity::Medium);
    }

    #[test]
    fn timestamps_span_the_group_and_message_is_surfaced() {
        let mut first = event(
            "payment_failed",
            "/checkout",
            "p-1",
            "2026-08-08T10:00:00.000Z",
        );
        first["properties"] =
            serde_json::json!({ "message": "card declined", "code": "insufficient_funds" });
        let input = envelope(
            "events",
            payload(vec![
                event(
                    "payment_failed",
                    "/checkout",
                    "p-2",
                    "2026-08-08T12:00:00.000Z",
                ),
                first,
            ]),
        );
        let signals = OpenpanelAdapter.normalize(&input).unwrap();
        let signal = &signals[0];
        assert_eq!(
            signal.body,
            "2 'payment_failed' event(s) on /checkout from 2 distinct user(s); \
             example message: card declined"
        );
        assert_eq!(signal.first_seen.to_rfc3339(), "2026-08-08T10:00:00+00:00");
        assert_eq!(signal.last_seen.to_rfc3339(), "2026-08-08T12:00:00+00:00");
        assert_eq!(
            signal.evidence[0].url,
            "https://openpanel.example.com/acme/website/events?event=payment_failed"
        );
        assert_eq!(signal.evidence[0].label, "openpanel events payment_failed");
    }

    #[test]
    fn malformed_events_are_skipped_while_the_rest_normalize() {
        let mut no_name = event("form_error", "/signup", "p-1", "2026-08-08T10:00:00.000Z");
        no_name.as_object_mut().unwrap().remove("name");
        let mut bad_date = event("form_error", "/signup", "p-2", "2026-08-08T10:00:00.000Z");
        bad_date["createdAt"] = serde_json::json!("not-a-date");
        let input = envelope(
            "events",
            payload(vec![
                no_name,
                bad_date,
                event("form_error", "/signup", "p-3", "2026-08-08T11:00:00.000Z"),
            ]),
        );
        let signals = OpenpanelAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].affected_count, Some(1));
        assert_eq!(signals[0].join_keys.account_id.as_deref(), Some("p-3"));
    }

    #[test]
    fn fingerprint_is_stable_across_polls() {
        // Same (name, path), different events/profiles → same fingerprint.
        let a = &OpenpanelAdapter
            .normalize(&envelope(
                "events",
                payload(vec![event(
                    "api_error",
                    "/dashboard",
                    "p-1",
                    "2026-08-08T10:00:00.000Z",
                )]),
            ))
            .unwrap()[0];
        let b = &OpenpanelAdapter
            .normalize(&envelope(
                "events",
                payload(vec![event(
                    "api_error",
                    "/dashboard",
                    "p-2",
                    "2026-08-09T10:00:00.000Z",
                )]),
            ))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(
            a.fingerprint,
            fingerprint(Source::Openpanel, &["event", "api_error", "/dashboard"])
        );
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut extra = event("api_error", "/dashboard", "p-1", "2026-08-08T10:00:00.000Z");
        extra["some_future_openpanel_field"] = serde_json::json!({ "nested": true });
        let signals = OpenpanelAdapter
            .normalize(&envelope("events", payload(vec![extra])))
            .unwrap();
        // Raw is the group's events as an array.
        assert_eq!(
            signals[0].raw[0]["some_future_openpanel_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("profiles", serde_json::json!({ "data": [] }));
        assert!(matches!(
            OpenpanelAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
