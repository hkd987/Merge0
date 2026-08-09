//! Telemetry → Signals (PRD §5d): the meta-loop's ingestion half.
//!
//! Supported envelope endpoints:
//!
//! - `telemetry` — a serialized
//!   [`merge0_signal::TelemetrySnapshot`] as the payload, with
//!   `{"captured_at": "<rfc3339>"}` in the envelope context (no wall-clock
//!   reads here; the capture time travels with the payload).
//!
//! One `custom`-kind Signal is emitted per notable metric condition, in a
//! fixed order; a healthy window emits an empty vec (not an error):
//!
//! | condition | fingerprint parts |
//! |---|---|
//! | `gate_precision < 0.7` | `["gate_precision_low"]` |
//! | `merge_rate < 0.6` | `["merge_rate_low"]` |
//! | `dismissals["intended_behavior"] >= 3` | `["dismissals_intended_rising"]` |
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Fingerprints carry no window data**, so re-capturing the same
//!   condition next window dedupes onto the same stored signal and widens
//!   its seen-window — recurrence, not duplication.
//! - **Severity** is window health: `medium` when `gate_precision < 0.7` or
//!   `merge_rate < 0.6` (where present), otherwise `low`. The health flag
//!   applies to every signal in the batch — an intended-behavior trend in an
//!   otherwise healthy window is low-severity advice.
//! - **`first_seen == last_seen == captured_at`**: a snapshot is a point
//!   observation; the seen-window widens through re-ingestion.
//! - **Bodies carry the metric values** so downstream proposals stay
//!   evidence-linked without re-querying the store.
//! - **Boundaries are strict**: exactly 0.7 precision / 0.6 merge rate is
//!   on-target, `>= 3` intended-behavior dismissals trips the trend.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::telemetry::TelemetryCounts;
use merge0_signal::{
    fingerprint, JoinKeys, Severity, Signal, SignalKind, Source, TelemetrySnapshot,
};
use serde::Deserialize;
use ulid::Ulid;

/// Precision below this is a notable condition (PRD leading indicator ≥70%).
pub const GATE_PRECISION_TARGET: f64 = 0.7;
/// Merge rate below this is a notable condition (Phase 0 gate ≥60%).
pub const MERGE_RATE_TARGET: f64 = 0.6;
/// Intended-behavior dismissals at or above this count mark a rising trend.
pub const INTENDED_DISMISSALS_THRESHOLD: u64 = 3;

pub struct MetaAdapter;

#[derive(Debug, Deserialize)]
struct TelemetryContext {
    captured_at: DateTime<Utc>,
}

impl Adapter for MetaAdapter {
    fn source(&self) -> Source {
        Source::Meta
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        match envelope.endpoint.as_str() {
            "telemetry" => {
                let context: TelemetryContext = serde_json::from_value(envelope.context.clone())
                    .map_err(|e| {
                        AdapterError::Malformed(format!(
                            "telemetry context needs captured_at (rfc3339): {e}"
                        ))
                    })?;
                let snapshot: TelemetrySnapshot = serde_json::from_value(envelope.payload.clone())
                    .map_err(|e| {
                        AdapterError::Malformed(format!("payload is not a TelemetrySnapshot: {e}"))
                    })?;
                Ok(signals_from_snapshot(
                    &snapshot,
                    context.captured_at,
                    &envelope.payload,
                ))
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn signals_from_snapshot(
    snapshot: &TelemetrySnapshot,
    captured_at: DateTime<Utc>,
    raw: &serde_json::Value,
) -> Vec<Signal> {
    let severity = window_severity(snapshot);
    let counts = &snapshot.counts;
    let mut signals = Vec::new();

    if let Some(precision) = snapshot.gate_precision {
        if precision < GATE_PRECISION_TARGET {
            signals.push(meta_signal(
                &["gate_precision_low"],
                "Gate precision below target",
                format!(
                    "gate_precision = {precision:.3} (target >= {GATE_PRECISION_TARGET:.3}) \
                     over the {}-day window; approved: {}; dismissals by reason: {}",
                    counts.window_days,
                    counts.reports_approved,
                    dismissals_by_reason(counts),
                ),
                severity,
                None,
                captured_at,
                raw,
            ));
        }
    }

    if let Some(merge_rate) = snapshot.merge_rate {
        if merge_rate < MERGE_RATE_TARGET {
            signals.push(meta_signal(
                &["merge_rate_low"],
                "Merge rate below target",
                format!(
                    "merge_rate = {merge_rate:.3} (target >= {MERGE_RATE_TARGET:.3}) over \
                     the {}-day window; merged: {}, closed: {}, reverted: {}",
                    counts.window_days, counts.prs_merged, counts.prs_closed, counts.prs_reverted,
                ),
                severity,
                None,
                captured_at,
                raw,
            ));
        }
    }

    let intended = intended_dismissals(counts);
    if intended >= INTENDED_DISMISSALS_THRESHOLD {
        signals.push(meta_signal(
            &["dismissals_intended_rising"],
            "Intended-behavior dismissals rising",
            format!(
                "dismissals[\"intended_behavior\"] = {intended} (threshold >= \
                 {INTENDED_DISMISSALS_THRESHOLD}) over the {}-day window",
                counts.window_days,
            ),
            severity,
            Some(intended),
            captured_at,
            raw,
        ));
    }

    signals
}

/// Window health → severity for every signal in the batch (see module docs).
fn window_severity(snapshot: &TelemetrySnapshot) -> Severity {
    let unhealthy = snapshot
        .gate_precision
        .is_some_and(|p| p < GATE_PRECISION_TARGET)
        || snapshot.merge_rate.is_some_and(|r| r < MERGE_RATE_TARGET);
    if unhealthy {
        Severity::Medium
    } else {
        Severity::Low
    }
}

pub(crate) fn intended_dismissals(counts: &TelemetryCounts) -> u64 {
    counts
        .dismissals
        .get(merge0_signal::DismissReason::IntendedBehavior.as_str())
        .copied()
        .unwrap_or(0)
}

pub(crate) fn dismissals_by_reason(counts: &TelemetryCounts) -> String {
    if counts.dismissals.is_empty() {
        return "none".to_string();
    }
    counts
        .dismissals
        .iter()
        .map(|(reason, n)| format!("{reason}: {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(clippy::too_many_arguments)]
fn meta_signal(
    parts: &[&str],
    title: &str,
    body: String,
    severity: Severity,
    affected_count: Option<u64>,
    captured_at: DateTime<Utc>,
    raw: &serde_json::Value,
) -> Signal {
    Signal {
        id: Ulid::new(),
        source: Source::Meta,
        source_ref: format!("telemetry:{}", captured_at.to_rfc3339()),
        kind: SignalKind::Custom,
        severity,
        title: title.to_string(),
        body,
        evidence: vec![],
        fingerprint: fingerprint(Source::Meta, parts),
        join_keys: JoinKeys::default(),
        affected_count,
        delegated: false,
        first_seen: captured_at,
        last_seen: captured_at,
        raw: raw.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn snapshot(
        merged: u64,
        closed: u64,
        approved: u64,
        dismissals: &[(&str, u64)],
    ) -> TelemetrySnapshot {
        TelemetrySnapshot::from_counts(TelemetryCounts {
            window_days: 30,
            prs_merged: merged,
            prs_closed: closed,
            reports_approved: approved,
            dismissals: dismissals
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<BTreeMap<_, _>>(),
            ..Default::default()
        })
    }

    fn captured_at() -> DateTime<Utc> {
        "2026-08-01T00:00:00Z".parse().unwrap()
    }

    fn normalize(snapshot: &TelemetrySnapshot) -> Vec<Signal> {
        let input = serde_json::json!({
            "endpoint": "telemetry",
            "context": { "captured_at": "2026-08-01T00:00:00Z" },
            "payload": serde_json::to_value(snapshot).unwrap(),
        });
        MetaAdapter.normalize(&input).unwrap()
    }

    #[test]
    fn healthy_window_emits_nothing() {
        // precision 9/11 ≈ 0.82, merge rate 0.8, intended dismissals 1.
        let snap = snapshot(8, 2, 9, &[("intended_behavior", 1), ("duplicate", 1)]);
        assert!(normalize(&snap).is_empty());
    }

    #[test]
    fn empty_snapshot_emits_nothing_and_is_not_an_error() {
        let snap = TelemetrySnapshot::from_counts(TelemetryCounts::default());
        assert!(normalize(&snap).is_empty());
    }

    #[test]
    fn low_gate_precision_emits_medium_signal_with_value_in_body() {
        // precision 3/8 = 0.375; merge rate absent; intended 2 (< 3).
        let snap = snapshot(0, 0, 3, &[("intended_behavior", 2), ("wont_fix", 3)]);
        let signals = normalize(&snap);
        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(s.source, Source::Meta);
        assert_eq!(s.kind, SignalKind::Custom);
        assert_eq!(s.severity, Severity::Medium);
        assert_eq!(
            s.fingerprint,
            fingerprint(Source::Meta, &["gate_precision_low"])
        );
        assert!(
            s.body.contains("0.375"),
            "body must carry the value: {}",
            s.body
        );
        assert!(s.body.contains("intended_behavior: 2"));
        assert_eq!(s.first_seen, captured_at());
        assert_eq!(s.last_seen, captured_at());
    }

    #[test]
    fn gate_precision_boundary_at_exactly_target_is_healthy() {
        // 7/10 = 0.7 exactly → on-target, no signal.
        let snap = snapshot(0, 0, 7, &[("duplicate", 3)]);
        assert_eq!(snap.gate_precision, Some(0.7));
        assert!(normalize(&snap).is_empty());
    }

    #[test]
    fn low_merge_rate_emits_medium_signal() {
        // merge rate 1/2 = 0.5; precision absent.
        let snap = snapshot(1, 1, 0, &[]);
        let signals = normalize(&snap);
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0].fingerprint,
            fingerprint(Source::Meta, &["merge_rate_low"])
        );
        assert_eq!(signals[0].severity, Severity::Medium);
        assert!(signals[0].body.contains("0.500"));
    }

    #[test]
    fn merge_rate_boundary_at_exactly_target_is_healthy() {
        // 6/10 = 0.6 exactly → no signal.
        let snap = snapshot(6, 4, 0, &[]);
        assert_eq!(snap.merge_rate, Some(0.6));
        assert!(normalize(&snap).is_empty());
    }

    #[test]
    fn intended_dismissals_alone_are_low_severity() {
        // Rates healthy (precision 7/10 = 0.7, merge rate 1.0) but 3
        // intended-behavior dismissals → one low-severity trend signal.
        let snap = snapshot(10, 0, 7, &[("intended_behavior", 3)]);
        let signals = normalize(&snap);
        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(
            s.fingerprint,
            fingerprint(Source::Meta, &["dismissals_intended_rising"])
        );
        assert_eq!(s.severity, Severity::Low);
        assert_eq!(s.affected_count, Some(3));
        assert!(s.body.contains("= 3"));
    }

    #[test]
    fn intended_dismissals_below_threshold_are_quiet() {
        let snap = snapshot(10, 0, 7, &[("intended_behavior", 2)]);
        assert!(normalize(&snap).is_empty());
    }

    #[test]
    fn unhealthy_window_emits_all_tripped_conditions_in_order() {
        // precision 4/8 = 0.5, merge rate 4/8 = 0.5, intended 3.
        let snap = snapshot(4, 4, 4, &[("intended_behavior", 3), ("duplicate", 1)]);
        let signals = normalize(&snap);
        assert_eq!(signals.len(), 3);
        assert_eq!(
            signals[0].fingerprint,
            fingerprint(Source::Meta, &["gate_precision_low"])
        );
        assert_eq!(
            signals[1].fingerprint,
            fingerprint(Source::Meta, &["merge_rate_low"])
        );
        assert_eq!(
            signals[2].fingerprint,
            fingerprint(Source::Meta, &["dismissals_intended_rising"])
        );
        // Unhealthy window → the trend signal is medium too.
        assert!(signals.iter().all(|s| s.severity == Severity::Medium));
    }

    #[test]
    fn fingerprints_are_stable_across_windows() {
        let a = normalize(&snapshot(1, 1, 0, &[]));
        let mut later = snapshot(1, 3, 0, &[]);
        later.counts.window_days = 7;
        let input = serde_json::json!({
            "endpoint": "telemetry",
            "context": { "captured_at": "2026-08-08T00:00:00Z" },
            "payload": serde_json::to_value(&later).unwrap(),
        });
        let b = MetaAdapter.normalize(&input).unwrap();
        assert_eq!(a[0].fingerprint, b[0].fingerprint);
        assert_ne!(a[0].last_seen, b[0].last_seen);
    }

    #[test]
    fn missing_captured_at_is_malformed_not_a_panic() {
        let input = serde_json::json!({
            "endpoint": "telemetry",
            "payload": serde_json::to_value(snapshot(1, 1, 0, &[])).unwrap(),
        });
        assert!(matches!(
            MetaAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn malformed_payload_is_an_error_not_a_panic() {
        let input = serde_json::json!({
            "endpoint": "telemetry",
            "context": { "captured_at": "2026-08-01T00:00:00Z" },
            "payload": { "not": "a snapshot" },
        });
        assert!(matches!(
            MetaAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = serde_json::json!({ "endpoint": "metrics", "payload": {} });
        assert!(matches!(
            MetaAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
