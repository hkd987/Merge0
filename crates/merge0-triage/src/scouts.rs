//! Scouts (PRD §4): standing questions defined as config, not code.
//!
//! Each enabled scout selects candidate signals by its configured `sources`
//! over its schedule window, filtered and ordered by its executed
//! `query_template` (see [`crate::query`]); the union (deduplicated) is
//! clustering's input. The scout `prompt` travels into gate context as
//! provenance. A model-driven scout runtime (prompt over the selected
//! signals → findings) slots in behind the same config without changes to
//! the files in `config/scouts/` — which is the point of config-as-files.

use crate::config::ScoutConfig;
use crate::query::Query;
use crate::TriageError;
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

/// Parse a scout's `query_template`, attributing errors to the scout.
pub fn parse_query(scout: &ScoutConfig) -> crate::Result<Query> {
    Query::parse(&scout.query_template)
        .map_err(|e| TriageError::Config(format!("scout {:?} query_template: {e}", scout.name)))
}

/// Select the candidate set for one scout from pre-fetched recent signals:
/// enabled + schedule window + sources + executed query, in query order.
pub fn select<'a>(
    scout: &ScoutConfig,
    query: &Query,
    signals: &'a [Signal],
    now: DateTime<Utc>,
) -> Vec<&'a Signal> {
    if !scout.enabled {
        return Vec::new();
    }
    let cutoff = now - schedule_window(&scout.schedule);
    let mut selection: Vec<&Signal> = signals
        .iter()
        .filter(|s| {
            s.last_seen >= cutoff && scout.sources.contains(&s.source) && query.matches(s, cutoff)
        })
        .collect();
    query.apply_order(&mut selection);
    selection
}

/// Union of all scouts' selections, deduplicated by signal id, selection
/// order preserved. A malformed `query_template` fails the whole run
/// (fail-closed — a typo must not silently change what scouts see).
pub fn union_candidates<'a>(
    scouts: &[ScoutConfig],
    signals: &'a [Signal],
    now: DateTime<Utc>,
) -> crate::Result<Vec<&'a Signal>> {
    let mut seen = std::collections::HashSet::new();
    let mut selected = Vec::new();
    for scout in scouts {
        let query = parse_query(scout)?;
        for signal in select(scout, &query, signals, now) {
            if seen.insert(signal.id) {
                selected.push(signal);
            }
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use merge0_signal::{JoinKeys, Severity, SignalKind, Source};
    use ulid::Ulid;

    fn scout(sources: &[Source], schedule: &str, enabled: bool) -> ScoutConfig {
        scout_with_query(sources, schedule, enabled, "")
    }

    fn scout_with_query(
        sources: &[Source],
        schedule: &str,
        enabled: bool,
        query_template: &str,
    ) -> ScoutConfig {
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
            query_template = "{query_template}"
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
            delegated: false,
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
        let query = parse_query(&s).unwrap();
        let picked = select(&s, &query, &signals, now());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, fresh_sentry.id);

        let disabled = scout(&[Source::Sentry], "nightly", false);
        assert!(select(&disabled, &query, &signals, now()).is_empty());
    }

    #[test]
    fn executed_query_narrows_selection_and_orders_it() {
        // Two fresh sentry signals; the query keeps only exceptions and
        // orders by affected_count DESC.
        let mut quiet = signal(Source::Sentry, now() - Duration::hours(1));
        quiet.affected_count = Some(2);
        let mut loud = signal(Source::Sentry, now() - Duration::hours(1));
        loud.affected_count = Some(50);
        let mut ticket = signal(Source::Sentry, now() - Duration::hours(1));
        ticket.kind = SignalKind::Ticket;
        let signals = vec![quiet.clone(), loud.clone(), ticket];

        let s = scout_with_query(
            &[Source::Sentry],
            "nightly",
            true,
            "kind = 'exception' ORDER BY affected_count DESC",
        );
        let query = parse_query(&s).unwrap();
        let picked = select(&s, &query, &signals, now());
        assert_eq!(picked.len(), 2, "the ticket is filtered out");
        assert_eq!(picked[0].id, loud.id, "highest affected_count first");
    }

    #[test]
    fn malformed_query_fails_the_union_loudly() {
        let s = scout_with_query(&[Source::Sentry], "nightly", true, "kindd = 'oops'");
        let err = union_candidates(&[s], &[], now()).expect_err("must fail");
        assert!(err.to_string().contains("query_template"), "{err}");
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
        let union = union_candidates(&scouts, &signals, now()).unwrap();
        assert_eq!(union.len(), 2, "signal picked by two scouts appears once");
    }
}
