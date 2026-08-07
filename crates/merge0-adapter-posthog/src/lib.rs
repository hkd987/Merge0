//! PostHog → Signals (PRD P0-1).
//!
//! Supported envelope endpoints:
//!
//! - `error_tracking_issues` — PostHog error tracking issue list
//!   (`{"results": [...]}`), one `exception` Signal per issue.
//! - `rageclick_events` — PostHog events API results for `$rageclick`
//!   (`{"results": [...]}`), aggregated into one `ux_friction` Signal per
//!   URL path.
//!
//! Envelope context: `{"project_base_url": "https://us.posthog.com/project/1"}`
//! — used to build deep links (issue pages, replay URLs).
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity for error issues** comes from affected-user count:
//!   ≥100 critical, ≥20 high, ≥5 medium, else (or unknown) low. Conservative
//!   by design — a quiet inbox that's right beats a busy one.
//! - **Severity for rage-click groups**: ≥3 distinct users medium, else low.
//! - **`join_keys.stack_hash`** is the hash of the exception type only, so it
//!   joins with the Sentry adapter's hash of `metadata.type` for the same
//!   defect class.
//! - **`join_keys.account_id`** on aggregated rage-click Signals is set only
//!   when exactly one distinct user is involved.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, stack_hash, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind,
    Source,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use ulid::Ulid;

pub struct PosthogAdapter;

/// Typed context for the PostHog envelope.
#[derive(Debug, Deserialize)]
struct Context {
    project_base_url: String,
}

#[derive(Debug, Deserialize)]
struct ResultsPage {
    results: Vec<serde_json::Value>,
}

/// PostHog error tracking issue — only the fields we normalize; everything
/// else is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct ErrorTrackingIssue {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    #[serde(default)]
    users: Option<u64>,
}

/// A `$rageclick` event from the PostHog events API.
#[derive(Debug, Deserialize)]
struct RageclickEvent {
    distinct_id: String,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    properties: RageclickProperties,
}

#[derive(Debug, Default, Deserialize)]
struct RageclickProperties {
    #[serde(rename = "$pathname", default)]
    pathname: Option<String>,
    #[serde(rename = "$current_url", default)]
    current_url: Option<String>,
    #[serde(rename = "$session_id", default)]
    session_id: Option<String>,
}

impl Adapter for PosthogAdapter {
    fn source(&self) -> Source {
        Source::Posthog
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid posthog context: {e}")))?;
        let base_url = context.project_base_url.trim_end_matches('/').to_string();

        let page: ResultsPage = serde_json::from_value(envelope.payload.clone()).map_err(|e| {
            AdapterError::Malformed(format!("expected {{\"results\": [...]}}: {e}"))
        })?;

        match envelope.endpoint.as_str() {
            "error_tracking_issues" => page
                .results
                .iter()
                .map(|issue| normalize_issue(issue, &base_url))
                .collect(),
            "rageclick_events" => normalize_rageclicks(&page.results, &base_url),
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_issue(raw: &serde_json::Value, base_url: &str) -> Result<Signal, AdapterError> {
    let issue: ErrorTrackingIssue = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid error tracking issue: {e}")))?;

    Ok(Signal {
        id: Ulid::new(),
        source: Source::Posthog,
        source_ref: issue.id.clone(),
        kind: SignalKind::Exception,
        severity: severity_from_impact(issue.users),
        title: issue.name.clone(),
        body: issue.description.clone().unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: format!("PostHog issue {}", issue.name),
            url: format!("{base_url}/error_tracking/{}", issue.id),
        }],
        fingerprint: fingerprint(Source::Posthog, &["error_tracking_issue", &issue.id]),
        join_keys: JoinKeys {
            stack_hash: Some(stack_hash(&[issue.name.trim()])),
            ..Default::default()
        },
        affected_count: issue.users,
        first_seen: issue.first_seen,
        last_seen: issue.last_seen,
        raw: raw.clone(),
    })
}

fn normalize_rageclicks(
    events: &[serde_json::Value],
    base_url: &str,
) -> Result<Vec<Signal>, AdapterError> {
    struct Group {
        raws: Vec<serde_json::Value>,
        users: Vec<String>,
        sessions: Vec<String>,
        first_seen: DateTime<Utc>,
        last_seen: DateTime<Utc>,
    }

    // BTreeMap keyed by path → deterministic output order.
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();

    for raw in events {
        let event: RageclickEvent = serde_json::from_value(raw.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid rageclick event: {e}")))?;
        let path = event
            .properties
            .pathname
            .clone()
            .or_else(|| event.properties.current_url.as_deref().map(url_path))
            .ok_or_else(|| {
                AdapterError::Malformed(
                    "rageclick event has neither $pathname nor $current_url".into(),
                )
            })?;

        let group = groups.entry(path).or_insert_with(|| Group {
            raws: vec![],
            users: vec![],
            sessions: vec![],
            first_seen: event.timestamp,
            last_seen: event.timestamp,
        });
        group.raws.push(raw.clone());
        if !group.users.contains(&event.distinct_id) {
            group.users.push(event.distinct_id.clone());
        }
        if let Some(session) = &event.properties.session_id {
            if !group.sessions.contains(session) {
                group.sessions.push(session.clone());
            }
        }
        group.first_seen = group.first_seen.min(event.timestamp);
        group.last_seen = group.last_seen.max(event.timestamp);
    }

    Ok(groups
        .into_iter()
        .map(|(path, group)| {
            let user_count = group.users.len() as u64;
            let severity = if user_count >= 3 {
                Severity::Medium
            } else {
                Severity::Low
            };
            // At most 3 replay links; over-budget evidence stays reachable
            // through the source deep links in `raw`.
            let evidence = group
                .sessions
                .iter()
                .take(3)
                .map(|session| EvidenceLink {
                    kind: EvidenceKind::Replay,
                    label: format!("Session replay {session}"),
                    url: format!("{base_url}/replay/{session}"),
                })
                .collect();
            let account_id = if group.users.len() == 1 {
                Some(group.users[0].clone())
            } else {
                None
            };
            Signal {
                id: Ulid::new(),
                source: Source::Posthog,
                source_ref: format!("rageclick:{path}"),
                kind: SignalKind::UxFriction,
                severity,
                title: format!("Rage clicks on {path}"),
                body: format!(
                    "{} rage-click event(s) from {} distinct user(s) on {path}",
                    group.raws.len(),
                    user_count
                ),
                evidence,
                fingerprint: fingerprint(Source::Posthog, &["rageclick", &path]),
                join_keys: JoinKeys {
                    account_id,
                    url_path: Some(path),
                    ..Default::default()
                },
                affected_count: Some(user_count),
                first_seen: group.first_seen,
                last_seen: group.last_seen,
                raw: serde_json::Value::Array(group.raws),
            }
        })
        .collect())
}

/// Impact-based severity for error tracking issues (see module docs).
fn severity_from_impact(users: Option<u64>) -> Severity {
    match users {
        Some(n) if n >= 100 => Severity::Critical,
        Some(n) if n >= 20 => Severity::High,
        Some(n) if n >= 5 => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Extract the path component from a URL without pulling in a URL crate:
/// strip scheme+host, drop query/fragment. Host-only URLs map to `/`.
fn url_path(url: &str) -> String {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => "/",
    };
    let end = path.find(['?', '#']).unwrap_or(path.len());
    path[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity_from_impact(None), Severity::Low);
        assert_eq!(severity_from_impact(Some(4)), Severity::Low);
        assert_eq!(severity_from_impact(Some(5)), Severity::Medium);
        assert_eq!(severity_from_impact(Some(19)), Severity::Medium);
        assert_eq!(severity_from_impact(Some(20)), Severity::High);
        assert_eq!(severity_from_impact(Some(99)), Severity::High);
        assert_eq!(severity_from_impact(Some(100)), Severity::Critical);
    }

    #[test]
    fn url_path_extraction() {
        assert_eq!(
            url_path("https://app.example.com/districts/sync?tab=1"),
            "/districts/sync"
        );
        assert_eq!(url_path("https://app.example.com"), "/");
        assert_eq!(url_path("https://app.example.com/"), "/");
        assert_eq!(url_path("https://app.example.com/a#frag"), "/a");
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "project_base_url": "https://us.posthog.com/project/1" },
            "payload": payload,
        })
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        // Same issue id, different counts/last_seen → same fingerprint.
        let a = envelope(
            "error_tracking_issues",
            serde_json::json!({ "results": [{
                "id": "issue-1", "name": "TypeError",
                "first_seen": "2026-08-01T00:00:00Z", "last_seen": "2026-08-02T00:00:00Z",
                "users": 3
            }]}),
        );
        let b = envelope(
            "error_tracking_issues",
            serde_json::json!({ "results": [{
                "id": "issue-1", "name": "TypeError",
                "first_seen": "2026-08-01T00:00:00Z", "last_seen": "2026-08-06T00:00:00Z",
                "users": 250
            }]}),
        );
        let sig_a = &PosthogAdapter.normalize(&a).unwrap()[0];
        let sig_b = &PosthogAdapter.normalize(&b).unwrap()[0];
        assert_eq!(sig_a.fingerprint, sig_b.fingerprint);
        // ...while volatile facts still differ.
        assert_ne!(sig_a.severity, sig_b.severity);
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let input = envelope(
            "error_tracking_issues",
            serde_json::json!({ "results": [{
                "id": "issue-1", "name": "TypeError",
                "first_seen": "2026-08-01T00:00:00Z", "last_seen": "2026-08-02T00:00:00Z",
                "some_future_posthog_field": { "nested": true }
            }]}),
        );
        let signals = PosthogAdapter.normalize(&input).unwrap();
        assert_eq!(
            signals[0].raw["some_future_posthog_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn malformed_issue_is_an_error_not_a_panic() {
        // Missing required `id`.
        let input = envelope(
            "error_tracking_issues",
            serde_json::json!({ "results": [{ "name": "TypeError",
                "first_seen": "2026-08-01T00:00:00Z", "last_seen": "2026-08-02T00:00:00Z" }]}),
        );
        assert!(matches!(
            PosthogAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("funnels", serde_json::json!({ "results": [] }));
        assert!(matches!(
            PosthogAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }

    #[test]
    fn rageclicks_group_by_path_with_account_id_only_for_single_user() {
        let input = envelope(
            "rageclick_events",
            serde_json::json!({ "results": [
                { "distinct_id": "u1", "timestamp": "2026-08-01T10:00:00Z",
                  "properties": { "$pathname": "/reports", "$session_id": "s1" } },
                { "distinct_id": "u2", "timestamp": "2026-08-01T11:00:00Z",
                  "properties": { "$pathname": "/reports", "$session_id": "s2" } },
                { "distinct_id": "u1", "timestamp": "2026-08-01T12:00:00Z",
                  "properties": { "$current_url": "https://app.example.com/settings?x=1" } }
            ]}),
        );
        let signals = PosthogAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 2);
        // BTreeMap order: /reports before /settings.
        assert_eq!(signals[0].join_keys.url_path.as_deref(), Some("/reports"));
        assert_eq!(signals[0].affected_count, Some(2));
        assert_eq!(signals[0].join_keys.account_id, None);
        assert_eq!(signals[1].join_keys.url_path.as_deref(), Some("/settings"));
        assert_eq!(signals[1].join_keys.account_id.as_deref(), Some("u1"));
    }
}
