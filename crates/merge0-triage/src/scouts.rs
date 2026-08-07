//! Scouts (PRD §4): standing questions defined as config, not code.
//!
//! v1 semantics: each enabled scout selects candidate signals by its
//! configured `sources` over its schedule window; the union (deduplicated)
//! is clustering's input. The scout `prompt` travels into gate context as
//! provenance. A model-driven scout runtime (prompt over the selected
//! signals → findings) slots in behind the same config without changes to
//! the files in `config/scouts/` — which is the point of config-as-files.

use crate::config::ScoutConfig;
use chrono::{DateTime, Duration, Utc};
use merge0_signal::Signal;

/// Window implied by a scout schedule label. Unknown labels get the nightly
/// window (conservative: nothing silently widens).
pub fn schedule_window(schedule: &str) -> Duration {
    match schedule {
        "hourly" => Duration::hours(1),
        "nightly" => Duration::hours(24),
        "weekly" => Duration::hours(24 * 7),
        _ => Duration::hours(24),
    }
}

/// Select the candidate set for one scout from pre-fetched recent signals.
pub fn select<'a>(
    scout: &ScoutConfig,
    signals: &'a [Signal],
    now: DateTime<Utc>,
) -> Vec<&'a Signal> {
    if !scout.enabled {
        return Vec::new();
    }
    let cutoff = now - schedule_window(&scout.schedule);
    signals
        .iter()
        .filter(|s| s.last_seen >= cutoff && scout.sources.contains(&s.source))
        .collect()
}

/// Union of all scouts' selections, deduplicated by signal id, input order
/// preserved.
pub fn union_candidates<'a>(
    scouts: &[ScoutConfig],
    signals: &'a [Signal],
    now: DateTime<Utc>,
) -> Vec<&'a Signal> {
    let mut seen = std::collections::HashSet::new();
    let mut selected = Vec::new();
    for scout in scouts {
        for signal in select(scout, signals, now) {
            if seen.insert(signal.id) {
                selected.push(signal);
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use merge0_signal::{JoinKeys, Severity, SignalKind, Source};
    use ulid::Ulid;

    fn scout(sources: &[Source], schedule: &str, enabled: bool) -> ScoutConfig {
        let sources = sources
            .iter()
            .map(|s| format!("\"{}\"", s.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        toml::from_str(&format!(
            r#"
            name = "s"
            description = "d"
            schedule = "{schedule}"
            sources = [{sources}]
            query_template = "q"
            prompt = "p"
            enabled = {enabled}
            "#
        ))
        .unwrap()
    }

    fn signal(source: Source, last_seen: DateTime<Utc>) -> Signal {
        Signal {
            id: Ulid::new(),
            source,
            source_ref: "1".into(),
            kind: SignalKind::Exception,
            severity: Severity::High,
            title: "t".into(),
            body: String::new(),
            evidence: vec![],
            fingerprint: format!("{}:{}", source.as_str(), Ulid::new()),
            join_keys: JoinKeys::default(),
            affected_count: None,
            first_seen: last_seen,
            last_seen,
            raw: serde_json::Value::Null,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 0, 0, 0).unwrap()
    }

    #[test]
    fn schedule_windows() {
        assert_eq!(schedule_window("hourly"), Duration::hours(1));
        assert_eq!(schedule_window("nightly"), Duration::hours(24));
        assert_eq!(schedule_window("weekly"), Duration::hours(24 * 7));
        assert_eq!(schedule_window("someday"), Duration::hours(24));
    }

    #[test]
    fn select_filters_by_source_window_and_enabled() {
        let fresh_sentry = signal(Source::Sentry, now() - Duration::hours(2));
        let stale_sentry = signal(Source::Sentry, now() - Duration::hours(30));
        let fresh_posthog = signal(Source::Posthog, now() - Duration::hours(2));
        let signals = vec![fresh_sentry.clone(), stale_sentry, fresh_posthog];

        let s = scout(&[Source::Sentry], "nightly", true);
        let picked = select(&s, &signals, now());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, fresh_sentry.id);

        let disabled = scout(&[Source::Sentry], "nightly", false);
        assert!(select(&disabled, &signals, now()).is_empty());
    }

    #[test]
    fn union_deduplicates_across_scouts() {
        let both = signal(Source::Sentry, now() - Duration::hours(1));
        let posthog_only = signal(Source::Posthog, now() - Duration::hours(1));
        let signals = vec![both, posthog_only];
        let scouts = vec![
            scout(&[Source::Sentry, Source::Posthog], "nightly", true),
            scout(&[Source::Sentry], "nightly", true),
        ];
        let union = union_candidates(&scouts, &signals, now());
        assert_eq!(union.len(), 2, "signal picked by two scouts appears once");
    }
}
