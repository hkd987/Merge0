//! Sentry → Signals (PRD P0-2).
//!
//! Supported envelope endpoints:
//!
//! - `issues` — the Sentry issue list API response (a bare JSON array of
//!   issue objects), one `exception` Signal per issue.
//!
//! No envelope context is required: Sentry issue payloads carry their own
//! `permalink` for deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity** maps from the Sentry `level`: fatal → critical,
//!   error → high, warning → medium, info/debug → low. Unknown or absent
//!   level → medium (an issue Sentry couldn't classify still made it past
//!   grouping, so it is not defaulted to the floor).
//! - **`join_keys.release`** prefers `firstRelease.version` (the first-seen,
//!   i.e. first-bad-release candidate used by release-context attribution,
//!   PRD P0-4), falling back to `lastRelease.version`.
//! - **`join_keys.stack_hash`** is the hash of `metadata.type` (the exception
//!   type) only, so it joins with the PostHog adapter's hash of the issue
//!   name for the same defect class.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, stack_hash, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind,
    Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct SentryAdapter;

/// A Sentry issue — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
    id: String,
    #[serde(default)]
    short_id: Option<String>,
    title: String,
    #[serde(default)]
    culprit: Option<String>,
    permalink: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    user_count: Option<u64>,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    #[serde(default)]
    metadata: Metadata,
    #[serde(default)]
    first_release: Option<Release>,
    #[serde(default)]
    last_release: Option<Release>,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    #[serde(rename = "type", default)]
    exception_type: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    version: String,
}

impl Adapter for SentryAdapter {
    fn source(&self) -> Source {
        Source::Sentry
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "issues" => {
                let issues = envelope.payload.as_array().ok_or_else(|| {
                    AdapterError::Malformed("issues payload must be a JSON array".into())
                })?;
                issues.iter().map(normalize_issue).collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_issue(raw: &serde_json::Value) -> Result<Signal, AdapterError> {
    let issue: Issue = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid sentry issue: {e}")))?;

    let mut evidence = vec![EvidenceLink {
        kind: EvidenceKind::Issue,
        label: format!(
            "Sentry issue {}",
            issue.short_id.as_deref().unwrap_or(&issue.id)
        ),
        url: issue.permalink.clone(),
    }];
    evidence.push(EvidenceLink {
        kind: EvidenceKind::StackTrace,
        label: "Latest event".into(),
        url: latest_event_url(&issue.permalink),
    });

    let release = issue
        .first_release
        .as_ref()
        .or(issue.last_release.as_ref())
        .map(|r| r.version.clone());

    Ok(Signal {
        id: Ulid::generate(),
        source: Source::Sentry,
        source_ref: issue.id.clone(),
        kind: SignalKind::Exception,
        severity: severity_from_level(issue.level.as_deref()),
        title: issue.title.clone(),
        body: body_text(&issue),
        evidence,
        fingerprint: fingerprint(Source::Sentry, &["issue", &issue.id]),
        join_keys: JoinKeys {
            release,
            stack_hash: issue
                .metadata
                .exception_type
                .as_deref()
                .map(|t| stack_hash(&[t.trim()])),
            ..Default::default()
        },
        affected_count: issue.user_count,
        delegated: false,
        first_seen: issue.first_seen,
        last_seen: issue.last_seen,
        raw: raw.clone(),
    })
}

fn body_text(issue: &Issue) -> String {
    match (issue.metadata.value.as_deref(), issue.culprit.as_deref()) {
        (Some(value), Some(culprit)) if !culprit.is_empty() => format!("{value} (in {culprit})"),
        (Some(value), _) => value.to_string(),
        (None, Some(culprit)) if !culprit.is_empty() => format!("in {culprit}"),
        _ => String::new(),
    }
}

fn latest_event_url(permalink: &str) -> String {
    if permalink.ends_with('/') {
        format!("{permalink}events/latest/")
    } else {
        format!("{permalink}/events/latest/")
    }
}

/// Sentry `level` → severity (see module docs).
fn severity_from_level(level: Option<&str>) -> Severity {
    match level {
        Some("fatal") => Severity::Critical,
        Some("error") => Severity::High,
        Some("warning") => Severity::Medium,
        Some("info") | Some("debug") => Severity::Low,
        _ => Severity::Medium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_mapping() {
        assert_eq!(severity_from_level(Some("fatal")), Severity::Critical);
        assert_eq!(severity_from_level(Some("error")), Severity::High);
        assert_eq!(severity_from_level(Some("warning")), Severity::Medium);
        assert_eq!(severity_from_level(Some("info")), Severity::Low);
        assert_eq!(severity_from_level(Some("debug")), Severity::Low);
        assert_eq!(severity_from_level(Some("sample")), Severity::Medium);
        assert_eq!(severity_from_level(None), Severity::Medium);
    }

    #[test]
    fn latest_event_url_handles_trailing_slash() {
        assert_eq!(
            latest_event_url("https://sentry.example.com/issues/1/"),
            "https://sentry.example.com/issues/1/events/latest/"
        );
        assert_eq!(
            latest_event_url("https://sentry.example.com/issues/1"),
            "https://sentry.example.com/issues/1/events/latest/"
        );
    }

    fn envelope(payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "endpoint": "issues", "payload": payload })
    }

    fn minimal_issue() -> serde_json::Value {
        serde_json::json!({
            "id": "42",
            "title": "TypeError: x is undefined",
            "permalink": "https://sentry.example.com/organizations/acme/issues/42/",
            "firstSeen": "2026-08-01T00:00:00Z",
            "lastSeen": "2026-08-02T00:00:00Z"
        })
    }

    #[test]
    fn release_prefers_first_release_over_last() {
        let mut issue = minimal_issue();
        issue["firstRelease"] = serde_json::json!({ "version": "v1.0.0" });
        issue["lastRelease"] = serde_json::json!({ "version": "v1.4.0" });
        let signals = SentryAdapter
            .normalize(&envelope(serde_json::json!([issue])))
            .unwrap();
        assert_eq!(signals[0].join_keys.release.as_deref(), Some("v1.0.0"));

        let mut issue = minimal_issue();
        issue["lastRelease"] = serde_json::json!({ "version": "v1.4.0" });
        let signals = SentryAdapter
            .normalize(&envelope(serde_json::json!([issue])))
            .unwrap();
        assert_eq!(signals[0].join_keys.release.as_deref(), Some("v1.4.0"));
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        let mut updated = minimal_issue();
        updated["lastSeen"] = serde_json::json!("2026-08-06T00:00:00Z");
        updated["userCount"] = serde_json::json!(500);
        let a = &SentryAdapter
            .normalize(&envelope(serde_json::json!([minimal_issue()])))
            .unwrap()[0];
        let b = &SentryAdapter
            .normalize(&envelope(serde_json::json!([updated])))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn cross_source_stack_hash_matches_posthog_convention() {
        // Both adapters hash the bare exception type, so the same defect
        // class joins across sources (the whole point of join_keys).
        let mut issue = minimal_issue();
        issue["metadata"] = serde_json::json!({ "type": "TypeError", "value": "x is undefined" });
        let signals = SentryAdapter
            .normalize(&envelope(serde_json::json!([issue])))
            .unwrap();
        assert_eq!(
            signals[0].join_keys.stack_hash.as_deref(),
            Some(merge0_signal::stack_hash(&["TypeError"]).as_str())
        );
    }

    #[test]
    fn malformed_issue_is_an_error_not_a_panic() {
        // Missing required `permalink`.
        let issue = serde_json::json!({
            "id": "42",
            "title": "TypeError",
            "firstSeen": "2026-08-01T00:00:00Z",
            "lastSeen": "2026-08-02T00:00:00Z"
        });
        assert!(matches!(
            SentryAdapter.normalize(&envelope(serde_json::json!([issue]))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut issue = minimal_issue();
        issue["someFutureSentryField"] = serde_json::json!({ "nested": 1 });
        let signals = SentryAdapter
            .normalize(&envelope(serde_json::json!([issue])))
            .unwrap();
        assert_eq!(signals[0].raw["someFutureSentryField"]["nested"], 1);
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = serde_json::json!({ "endpoint": "releases", "payload": [] });
        assert!(matches!(
            SentryAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
