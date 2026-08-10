//! PostHog → Signals (PRD P0-1).
//!
//! Supported envelope endpoints:
//!
//! - `error_tracking_issues` — PostHog error tracking issue list
//!   (`{"results": [...]}`), one `exception` Signal per issue.
//! - `rageclick_events` — PostHog events API results for `$rageclick`
//!   (`{"results": [...]}`), aggregated into one `ux_friction` Signal per
//!   URL path.
//! - `dead_click_events` — same events-API shape for `$dead_click`, same
//!   per-path aggregation.
//! - `funnels` — funnel insight results (`{"results": [...]}`, one insight
//!   per entry as assembled by the poller), one `ux_friction` Signal per
//!   funnel with a meaningful worst-step drop.
//!
//! Envelope context: `{"project_base_url": "https://us.posthog.com/project/1"}`
//! — used to build deep links (issue pages, replay URLs).
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity for error issues** comes from affected-user count:
//!   ≥100 critical, ≥20 high, ≥5 medium, else (or unknown) low. Conservative
//!   by design — a quiet inbox that's right beats a busy one.
//! - **Severity for rage-click and dead-click groups**: ≥3 distinct users
//!   medium, else low.
//! - **Severity for funnel drop-offs** comes from the worst consecutive-step
//!   drop rate: ≥50% high, ≥25% medium, else low. Funnels whose worst drop
//!   is under 10% (or with fewer than two steps) produce no Signal at all —
//!   healthy funnels are not friction.
//! - **Funnel timestamps**: insight results are computed aggregates, not
//!   events, so `first_seen`/`last_seen` are both the insight's
//!   `last_refresh`.
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

/// A `$rageclick`/`$dead_click` event from the PostHog events API — the two
/// click-friction families share the same shape.
#[derive(Debug, Deserialize)]
struct ClickEvent {
    distinct_id: String,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    properties: ClickProperties,
}

#[derive(Debug, Default, Deserialize)]
struct ClickProperties {
    #[serde(rename = "$pathname", default)]
    pathname: Option<String>,
    #[serde(rename = "$current_url", default)]
    current_url: Option<String>,
    #[serde(rename = "$session_id", default)]
    session_id: Option<String>,
}

/// The copy/fingerprint identity of a click-friction event family.
struct ClickShape {
    /// Fingerprint + `source_ref` key (`rageclick:/path`).
    key: &'static str,
    /// Title prefix (`"Rage clicks on"` → `"Rage clicks on /path"`).
    title_prefix: &'static str,
    /// Body noun (`"rage-click"` → `"3 rage-click event(s) …"`).
    noun: &'static str,
}

const RAGECLICK: ClickShape = ClickShape {
    key: "rageclick",
    title_prefix: "Rage clicks on",
    noun: "rage-click",
};

const DEAD_CLICK: ClickShape = ClickShape {
    key: "dead_click",
    title_prefix: "Dead clicks on",
    noun: "dead-click",
};

/// A PostHog funnel insight, one entry of the `funnels` payload's
/// `results[]` (the poller assembles one entry per configured insight from
/// `/api/projects/{id}/insights/{insight_id}`).
#[derive(Debug, Deserialize)]
struct FunnelInsight {
    id: u64,
    #[serde(default)]
    short_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    last_refresh: DateTime<Utc>,
    /// The computed funnel steps, in order.
    result: Vec<FunnelStep>,
}

#[derive(Debug, Deserialize)]
struct FunnelStep {
    name: String,
    #[serde(default)]
    custom_name: Option<String>,
    /// Users who reached this step.
    count: u64,
    /// Page URL for pageview steps, when PostHog includes it.
    #[serde(default)]
    url: Option<String>,
}

impl FunnelInsight {
    /// UI slug for deep links and the fingerprint: `short_id` when present
    /// (what PostHog insight URLs use), else the numeric id.
    fn slug(&self) -> String {
        self.short_id.clone().unwrap_or_else(|| self.id.to_string())
    }

    fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("insight {}", self.slug()))
    }
}

impl FunnelStep {
    fn display_name(&self) -> &str {
        self.custom_name.as_deref().unwrap_or(&self.name)
    }
}

/// Worst-step drop floor below which a funnel yields no Signal (see module
/// docs).
const FUNNEL_MIN_DROP_RATE: f64 = 0.10;

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
            "rageclick_events" => normalize_click_events(&page.results, &base_url, &RAGECLICK),
            "dead_click_events" => normalize_click_events(&page.results, &base_url, &DEAD_CLICK),
            "funnels" => page
                .results
                .iter()
                .filter_map(|funnel| normalize_funnel(funnel, &base_url).transpose())
                .collect(),
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_issue(raw: &serde_json::Value, base_url: &str) -> Result<Signal, AdapterError> {
    let issue: ErrorTrackingIssue = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid error tracking issue: {e}")))?;

    Ok(Signal {
        id: Ulid::generate(),
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
        delegated: false,
        first_seen: issue.first_seen,
        last_seen: issue.last_seen,
        raw: raw.clone(),
    })
}

/// Funnel insight → at most one `ux_friction` Signal for its worst
/// consecutive-step drop; `None` when the funnel has no meaningful drop.
fn normalize_funnel(
    raw: &serde_json::Value,
    base_url: &str,
) -> Result<Option<Signal>, AdapterError> {
    let funnel: FunnelInsight = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid funnel insight: {e}")))?;

    // Worst consecutive-step drop: (step index, drop rate, users lost).
    let mut worst: Option<(usize, f64, u64)> = None;
    for (index, pair) in funnel.result.windows(2).enumerate() {
        let (entered, reached) = (pair[0].count, pair[1].count);
        if entered == 0 {
            continue;
        }
        let lost = entered.saturating_sub(reached);
        let rate = lost as f64 / entered as f64;
        if worst.is_none_or(|(_, worst_rate, _)| rate > worst_rate) {
            worst = Some((index + 1, rate, lost));
        }
    }
    let Some((step_index, rate, lost)) = worst else {
        return Ok(None); // Fewer than two steps (or all-zero entries).
    };
    if rate < FUNNEL_MIN_DROP_RATE {
        return Ok(None); // Healthy funnel — not friction.
    }

    let step = &funnel.result[step_index];
    let slug = funnel.slug();
    let percent = (rate * 100.0).round() as u64;
    let severity = if rate >= 0.50 {
        Severity::High
    } else if rate >= 0.25 {
        Severity::Medium
    } else {
        Severity::Low
    };

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Posthog,
        source_ref: format!("funnel:{slug}"),
        kind: SignalKind::UxFriction,
        severity,
        title: format!(
            "Funnel '{}': {percent}% drop at step '{}'",
            funnel.display_name(),
            step.display_name()
        ),
        body: format!(
            "{lost} of {} users lost at step {}/{} '{}' ({percent}% drop)",
            funnel.result[step_index - 1].count,
            step_index + 1,
            funnel.result.len(),
            step.display_name()
        ),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: format!("PostHog insight {}", funnel.display_name()),
            url: format!("{base_url}/insights/{slug}"),
        }],
        fingerprint: fingerprint(Source::Posthog, &["funnel", &slug]),
        join_keys: JoinKeys {
            url_path: step.url.as_deref().map(url_path),
            ..Default::default()
        },
        affected_count: Some(lost),
        delegated: false,
        first_seen: funnel.last_refresh,
        last_seen: funnel.last_refresh,
        raw: raw.clone(),
    }))
}

fn normalize_click_events(
    events: &[serde_json::Value],
    base_url: &str,
    shape: &ClickShape,
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
        let event: ClickEvent = serde_json::from_value(raw.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid {} event: {e}", shape.noun)))?;
        let path = event
            .properties
            .pathname
            .clone()
            .or_else(|| event.properties.current_url.as_deref().map(url_path))
            .ok_or_else(|| {
                AdapterError::Malformed(format!(
                    "{} event has neither $pathname nor $current_url",
                    shape.noun
                ))
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
                id: Ulid::generate(),
                source: Source::Posthog,
                source_ref: format!("{}:{path}", shape.key),
                kind: SignalKind::UxFriction,
                severity,
                title: format!("{} {path}", shape.title_prefix),
                body: format!(
                    "{} {} event(s) from {} distinct user(s) on {path}",
                    group.raws.len(),
                    shape.noun,
                    user_count
                ),
                evidence,
                fingerprint: fingerprint(Source::Posthog, &[shape.key, &path]),
                join_keys: JoinKeys {
                    account_id,
                    url_path: Some(path),
                    ..Default::default()
                },
                affected_count: Some(user_count),
                delegated: false,
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
        let input = envelope("surveys", serde_json::json!({ "results": [] }));
        assert!(matches!(
            PosthogAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }

    fn funnel(steps: serde_json::Value) -> serde_json::Value {
        envelope(
            "funnels",
            serde_json::json!({ "results": [{
                "id": 42,
                "name": "Signup",
                "last_refresh": "2026-08-05T06:00:00Z",
                "result": steps,
            }]}),
        )
    }

    fn step(name: &str, count: u64) -> serde_json::Value {
        serde_json::json!({ "name": name, "count": count })
    }

    #[test]
    fn funnel_severity_tracks_worst_step_drop_rate() {
        for (counts, expected) in [
            (vec![100, 50], Severity::High),       // 50% drop
            (vec![100, 75, 50], Severity::Medium), // worst 33%
            (vec![100, 89], Severity::Low),        // 11% drop
        ] {
            let steps: Vec<_> = counts
                .iter()
                .enumerate()
                .map(|(i, &c)| step(&format!("s{i}"), c))
                .collect();
            let signals = PosthogAdapter
                .normalize(&funnel(serde_json::Value::Array(steps)))
                .unwrap();
            assert_eq!(signals.len(), 1, "{counts:?}");
            assert_eq!(signals[0].severity, expected, "{counts:?}");
        }
    }

    #[test]
    fn healthy_and_degenerate_funnels_yield_no_signal() {
        for steps in [
            serde_json::json!([]),                              // no steps
            serde_json::json!([step("only", 100)]),             // one step
            serde_json::json!([step("a", 100), step("b", 95)]), // 5% < floor
            serde_json::json!([step("a", 0), step("b", 0)]),    // zero entries
        ] {
            assert_eq!(
                PosthogAdapter.normalize(&funnel(steps.clone())).unwrap(),
                vec![],
                "{steps}"
            );
        }
    }

    #[test]
    fn funnel_signal_names_the_worst_step_and_counts_users_lost() {
        let input = funnel(serde_json::json!([
            { "name": "$pageview", "custom_name": "Visit signup", "count": 1000,
              "url": "https://app.example.com/signup" },
            { "name": "$pageview", "custom_name": "Confirm email", "count": 380,
              "url": "https://app.example.com/signup/confirm?src=email" },
            { "name": "onboarding_completed", "count": 350 },
        ]));
        let signals = PosthogAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(
            signal.title,
            "Funnel 'Signup': 62% drop at step 'Confirm email'"
        );
        assert_eq!(signal.affected_count, Some(620));
        assert_eq!(
            signal.join_keys.url_path.as_deref(),
            Some("/signup/confirm"),
            "url_path derives from the worst step's URL"
        );
        assert_eq!(signal.source_ref, "funnel:42");
        assert_eq!(
            signal.evidence[0].url,
            "https://us.posthog.com/project/1/insights/42"
        );
    }

    #[test]
    fn funnel_fingerprint_prefers_short_id_and_survives_count_changes() {
        let with_counts = |a: u64, b: u64| {
            envelope(
                "funnels",
                serde_json::json!({ "results": [{
                    "id": 42, "short_id": "AbCd1234",
                    "last_refresh": "2026-08-05T06:00:00Z",
                    "result": [step("a", a), step("b", b)],
                }]}),
            )
        };
        let sig_a = &PosthogAdapter.normalize(&with_counts(100, 40)).unwrap()[0];
        let sig_b = &PosthogAdapter.normalize(&with_counts(500, 100)).unwrap()[0];
        assert_eq!(sig_a.fingerprint, sig_b.fingerprint);
        assert_eq!(sig_a.source_ref, "funnel:AbCd1234");
        assert!(sig_a.evidence[0].url.ends_with("/insights/AbCd1234"));
    }

    #[test]
    fn dead_clicks_mirror_rageclick_grouping_with_their_own_fingerprint() {
        let events = serde_json::json!({ "results": [
            { "distinct_id": "u1", "timestamp": "2026-08-01T10:00:00Z",
              "properties": { "$pathname": "/reports", "$session_id": "s1" } },
            { "distinct_id": "u2", "timestamp": "2026-08-01T11:00:00Z",
              "properties": { "$pathname": "/reports" } }
        ]});
        let dead = PosthogAdapter
            .normalize(&envelope("dead_click_events", events.clone()))
            .unwrap();
        let rage = PosthogAdapter
            .normalize(&envelope("rageclick_events", events))
            .unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].title, "Dead clicks on /reports");
        assert_eq!(dead[0].source_ref, "dead_click:/reports");
        assert_eq!(dead[0].kind, SignalKind::UxFriction);
        assert_eq!(dead[0].affected_count, Some(2));
        assert_eq!(dead[0].join_keys.url_path.as_deref(), Some("/reports"));
        // Same page, different friction family → different fingerprints.
        assert_ne!(dead[0].fingerprint, rage[0].fingerprint);
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
