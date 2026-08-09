//! Mixpanel → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `funnels` — Mixpanel funnels Query API responses as assembled by the
//!   poller (`{"results": [{"funnel_id", "name", "fetched_at", "response"}]}`,
//!   one entry per configured saved funnel, `response` verbatim from
//!   `/api/2.0/funnels`), one `ux_friction` Signal per funnel with a
//!   meaningful worst-step drop.
//!
//! Envelope context:
//! `{"project_base_url": "https://mixpanel.example.com/project/318"}` — used
//! to build funnel deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Only the latest date** of the response is normalized: the lexicographic
//!   max of `meta.dates`, which is chronological for `yyyy-mm-dd` keys. Older
//!   dates are context the poller happened to fetch, not fresh friction.
//! - **Severity for funnel drop-offs** comes from the worst consecutive-step
//!   drop rate: ≥50% high, ≥25% medium, else low. Funnels whose worst drop
//!   is under 10% (or with fewer than two steps) produce no Signal at all —
//!   healthy funnels are not friction.
//! - **A malformed funnel entry yields no Signal** rather than failing the
//!   envelope: one stale or half-materialized saved funnel must not block the
//!   other funnels in the same poll.
//! - **Funnel timestamps**: funnel responses are computed aggregates, not
//!   events, so `first_seen`/`last_seen` are both the entry's `fetched_at`.
//! - **`join_keys` stays empty** by design: funnel aggregates carry no URL
//!   path, user identity, release, or stack identity to join on.
//! - **`affected_count`** is the absolute number of users lost at the worst
//!   step (previous step's count minus the step's count).

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use ulid::Ulid;

pub struct MixpanelAdapter;

/// Typed context for the Mixpanel envelope.
#[derive(Debug, Deserialize)]
struct Context {
    project_base_url: String,
}

#[derive(Debug, Deserialize)]
struct ResultsPage {
    results: Vec<serde_json::Value>,
}

/// One entry of the `funnels` payload's `results[]` — the poller wraps each
/// configured saved funnel's verbatim Query API response with its identity.
#[derive(Debug, Deserialize)]
struct FunnelEntry {
    funnel_id: u64,
    name: String,
    fetched_at: DateTime<Utc>,
    response: FunnelResponse,
}

/// The verbatim Mixpanel funnels Query API response — only the fields we
/// normalize; everything else is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct FunnelResponse {
    meta: FunnelMeta,
    /// Per-date computed funnels. Only the latest date is normalized (module
    /// docs), so the other dates stay untyped.
    data: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct FunnelMeta {
    dates: Vec<String>,
}

/// One date's computed funnel.
#[derive(Debug, Deserialize)]
struct DateEntry {
    /// The funnel steps, in order.
    steps: Vec<FunnelStep>,
}

#[derive(Debug, Deserialize)]
struct FunnelStep {
    /// Users who reached this step.
    count: u64,
    /// Step display name — what the Mixpanel funnel UI shows.
    goal: String,
}

/// Worst-step drop floor below which a funnel yields no Signal (see module
/// docs).
const FUNNEL_MIN_DROP_RATE: f64 = 0.10;

impl Adapter for MixpanelAdapter {
    fn source(&self) -> Source {
        Source::Mixpanel
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid mixpanel context: {e}")))?;
        let base_url = context.project_base_url.trim_end_matches('/').to_string();

        let page: ResultsPage = serde_json::from_value(envelope.payload.clone()).map_err(|e| {
            AdapterError::Malformed(format!("expected {{\"results\": [...]}}: {e}"))
        })?;

        match envelope.endpoint.as_str() {
            "funnels" => Ok(page
                .results
                .iter()
                .filter_map(|funnel| normalize_funnel(funnel, &base_url))
                .collect()),
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Funnel entry → at most one `ux_friction` Signal for the worst
/// consecutive-step drop on the latest computed date; `None` when the funnel
/// is healthy, has fewer than two steps, or is malformed (module docs).
fn normalize_funnel(raw: &serde_json::Value, base_url: &str) -> Option<Signal> {
    let funnel: FunnelEntry = serde_json::from_value(raw.clone()).ok()?;
    // Latest date: lexicographic max of `meta.dates`, chronological for
    // yyyy-mm-dd keys regardless of the order Mixpanel lists them in.
    let latest = funnel.response.meta.dates.iter().max()?;
    let entry: DateEntry =
        serde_json::from_value(funnel.response.data.get(latest)?.clone()).ok()?;

    // Worst consecutive-step drop: (step index, drop rate, users lost).
    let mut worst: Option<(usize, f64, u64)> = None;
    for (index, pair) in entry.steps.windows(2).enumerate() {
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
    let (step_index, rate, lost) = worst?; // None: fewer than two steps (or all-zero entries).
    if rate < FUNNEL_MIN_DROP_RATE {
        return None; // Healthy funnel — not friction.
    }

    let step = &entry.steps[step_index];
    let funnel_id = funnel.funnel_id.to_string();
    let percent = (rate * 100.0).round() as u64;
    let severity = if rate >= 0.50 {
        Severity::High
    } else if rate >= 0.25 {
        Severity::Medium
    } else {
        Severity::Low
    };
    let steps_line = entry
        .steps
        .iter()
        .map(|s| format!("{} {}", s.goal, s.count))
        .collect::<Vec<_>>()
        .join(" → ");

    Some(Signal {
        id: Ulid::new(),
        source: Source::Mixpanel,
        source_ref: funnel_id.clone(),
        kind: SignalKind::UxFriction,
        severity,
        title: format!(
            "Funnel drop-off: {} loses {percent}% at {}",
            funnel.name, step.goal
        ),
        body: format!(
            "{lost} of {} users lost at step {}/{} '{}' ({percent}% drop); steps: {steps_line}",
            entry.steps[step_index - 1].count,
            step_index + 1,
            entry.steps.len(),
            step.goal
        ),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: format!("mixpanel funnel {}", funnel.name),
            url: format!("{base_url}/funnels/{funnel_id}"),
        }],
        fingerprint: fingerprint(Source::Mixpanel, &["funnel", &funnel_id]),
        join_keys: JoinKeys::default(),
        affected_count: Some(lost),
        delegated: false,
        first_seen: funnel.fetched_at,
        last_seen: funnel.fetched_at,
        raw: raw.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "project_base_url": "https://mixpanel.example.com/project/318" },
            "payload": payload,
        })
    }

    fn step(goal: &str, count: u64) -> serde_json::Value {
        serde_json::json!({
            "count": count,
            "goal": goal,
            "event": goal,
            "step_conv_ratio": 1,
            "overall_conv_ratio": 1,
            "avg_time": 10
        })
    }

    /// A single-date funnel entry, the common test case.
    fn funnel(steps: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "funnel_id": 301,
            "name": "Signup funnel",
            "fetched_at": "2026-08-08T12:00:00Z",
            "response": {
                "meta": { "dates": ["2026-08-08"] },
                "data": { "2026-08-08": { "steps": steps } }
            }
        })
    }

    fn payload(funnels: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "results": funnels })
    }

    #[test]
    fn severity_tracks_worst_step_drop_rate() {
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
            let signals = MixpanelAdapter
                .normalize(&envelope("funnels", payload(vec![funnel(steps)])))
                .unwrap();
            assert_eq!(signals.len(), 1, "{counts:?}");
            assert_eq!(signals[0].severity, expected, "{counts:?}");
        }
    }

    #[test]
    fn healthy_and_degenerate_funnels_yield_no_signal() {
        for steps in [
            vec![],                              // no steps
            vec![step("only", 100)],             // one step
            vec![step("a", 100), step("b", 95)], // 5% < floor
            vec![step("a", 0), step("b", 0)],    // zero entries
        ] {
            let input = envelope("funnels", payload(vec![funnel(steps.clone())]));
            assert_eq!(
                MixpanelAdapter.normalize(&input).unwrap(),
                vec![],
                "{steps:?}"
            );
        }
    }

    #[test]
    fn latest_date_wins_regardless_of_listing_order() {
        // Older date has a catastrophic drop, latest a moderate one — the
        // Signal must reflect the latest date only. `meta.dates` is listed
        // newest-first here to prove selection is by value, not position.
        let input = envelope(
            "funnels",
            payload(vec![serde_json::json!({
                "funnel_id": 301,
                "name": "Signup funnel",
                "fetched_at": "2026-08-08T12:00:00Z",
                "response": {
                    "meta": { "dates": ["2026-08-08", "2026-08-01"] },
                    "data": {
                        "2026-08-01": { "steps": [step("Landing", 1000), step("Signup", 400)] },
                        "2026-08-08": { "steps": [step("Landing", 500), step("Signup", 300)] }
                    }
                }
            })]),
        );
        let signals = MixpanelAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0].title,
            "Funnel drop-off: Signup funnel loses 40% at Signup"
        );
        assert_eq!(signals[0].affected_count, Some(200));
        assert_eq!(signals[0].severity, Severity::Medium);
    }

    #[test]
    fn funnel_signal_names_the_worst_step_and_counts_users_lost() {
        let input = envelope(
            "funnels",
            payload(vec![funnel(vec![
                step("Visit signup", 1000),
                step("Confirm email", 380),
            ])]),
        );
        let signals = MixpanelAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        let signal = &signals[0];
        assert_eq!(
            signal.title,
            "Funnel drop-off: Signup funnel loses 62% at Confirm email"
        );
        assert_eq!(
            signal.body,
            "620 of 1000 users lost at step 2/2 'Confirm email' (62% drop); \
             steps: Visit signup 1000 → Confirm email 380"
        );
        assert_eq!(signal.affected_count, Some(620));
        assert_eq!(signal.source_ref, "301");
        assert_eq!(signal.kind, SignalKind::UxFriction);
        assert!(signal.join_keys.is_empty(), "no join keys are derivable");
        assert_eq!(signal.first_seen, signal.last_seen);
        assert_eq!(signal.evidence[0].label, "mixpanel funnel Signup funnel");
        assert_eq!(
            signal.evidence[0].url,
            "https://mixpanel.example.com/project/318/funnels/301"
        );
    }

    #[test]
    fn fingerprint_is_stable_across_polls() {
        // Same funnel_id, different fetched_at/counts → same fingerprint.
        let with_counts = |fetched_at: &str, a: u64, b: u64| {
            envelope(
                "funnels",
                payload(vec![serde_json::json!({
                    "funnel_id": 301,
                    "name": "Signup funnel",
                    "fetched_at": fetched_at,
                    "response": {
                        "meta": { "dates": ["2026-08-08"] },
                        "data": { "2026-08-08": { "steps": [step("a", a), step("b", b)] } }
                    }
                })]),
            )
        };
        let sig_a = &MixpanelAdapter
            .normalize(&with_counts("2026-08-08T12:00:00Z", 100, 40))
            .unwrap()[0];
        let sig_b = &MixpanelAdapter
            .normalize(&with_counts("2026-08-09T12:00:00Z", 500, 100))
            .unwrap()[0];
        assert_eq!(sig_a.fingerprint, sig_b.fingerprint);
        assert_eq!(
            sig_a.fingerprint,
            fingerprint(Source::Mixpanel, &["funnel", "301"])
        );
    }

    #[test]
    fn malformed_entry_is_skipped_while_others_normalize() {
        for broken in [
            serde_json::json!({ "name": "No id", "fetched_at": "2026-08-08T12:00:00Z",
                "response": { "meta": { "dates": [] }, "data": {} } }),
            // Latest date missing from `data`.
            serde_json::json!({ "funnel_id": 999, "name": "Hole", "fetched_at": "2026-08-08T12:00:00Z",
                "response": { "meta": { "dates": ["2026-08-08"] }, "data": {} } }),
            // `steps` is not a list.
            serde_json::json!({ "funnel_id": 998, "name": "Bad steps", "fetched_at": "2026-08-08T12:00:00Z",
                "response": { "meta": { "dates": ["2026-08-08"] },
                              "data": { "2026-08-08": { "steps": "oops" } } } }),
        ] {
            let good = funnel(vec![step("a", 100), step("b", 40)]);
            let signals = MixpanelAdapter
                .normalize(&envelope("funnels", payload(vec![broken.clone(), good])))
                .unwrap();
            assert_eq!(signals.len(), 1, "{broken}");
            assert_eq!(signals[0].source_ref, "301");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut entry = funnel(vec![step("a", 100), step("b", 40)]);
        entry["some_future_mixpanel_field"] = serde_json::json!({ "nested": true });
        let signals = MixpanelAdapter
            .normalize(&envelope("funnels", payload(vec![entry])))
            .unwrap();
        assert_eq!(
            signals[0].raw["some_future_mixpanel_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("insights", serde_json::json!({ "results": [] }));
        assert!(matches!(
            MixpanelAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
