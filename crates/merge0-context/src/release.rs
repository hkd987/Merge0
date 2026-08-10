//! Release context: first-bad-release attribution (PRD P0-4).
//!
//! Attribution strategy, in order of confidence:
//! 1. The signals' own `join_keys.release` (most common release wins —
//!    vendor data beats timeline inference).
//! 2. Timeline inference: the release that was live when the cluster was
//!    first seen (latest release with `released_at <= first_seen`).

use chrono::{DateTime, Utc};
use merge0_signal::Signal;
use std::collections::BTreeMap;

/// The release live at `first_seen`, from an oldest→newest timeline.
pub fn release_at(
    timeline: &[(String, DateTime<Utc>)],
    first_seen: DateTime<Utc>,
) -> Option<String> {
    timeline
        .iter()
        .rfind(|(_, released_at)| *released_at <= first_seen)
        .map(|(version, _)| version.clone())
}

/// Suspect release for a cluster of signals.
pub fn suspect_release(signals: &[Signal], timeline: &[(String, DateTime<Utc>)]) -> Option<String> {
    // Majority vote over vendor-provided release join keys.
    let mut votes: BTreeMap<&str, usize> = BTreeMap::new();
    for signal in signals {
        if let Some(release) = signal.join_keys.release.as_deref() {
            *votes.entry(release).or_default() += 1;
        }
    }
    if let Some((release, _)) = votes.iter().max_by_key(|(release, n)| (**n, *release)) {
        return Some((*release).to_string());
    }
    // Fall back to timeline inference from the earliest first_seen.
    let earliest = signals.iter().map(|s| s.first_seen).min()?;
    release_at(timeline, earliest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use merge0_signal::{JoinKeys, Severity, SignalKind, Source};
    use ulid::Ulid;

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn signal(release: Option<&str>, first_seen: DateTime<Utc>) -> Signal {
        Signal {
            id: Ulid::generate(),
            source: Source::Sentry,
            source_ref: "1".into(),
            kind: SignalKind::Exception,
            severity: Severity::High,
            title: "t".into(),
            body: String::new(),
            evidence: vec![],
            fingerprint: "sentry:x".into(),
            join_keys: JoinKeys {
                release: release.map(String::from),
                ..Default::default()
            },
            affected_count: None,
            delegated: false,
            first_seen,
            last_seen: first_seen,
            raw: serde_json::Value::Null,
        }
    }

    fn timeline() -> Vec<(String, DateTime<Utc>)> {
        vec![
            ("v1.0.0".into(), ts(1)),
            ("v2.0.0".into(), ts(5)),
            ("v3.0.0".into(), ts(10)),
        ]
    }

    #[test]
    fn release_at_picks_latest_release_before_first_seen() {
        assert_eq!(release_at(&timeline(), ts(6)), Some("v2.0.0".into()));
        assert_eq!(release_at(&timeline(), ts(5)), Some("v2.0.0".into()));
        assert_eq!(release_at(&timeline(), ts(20)), Some("v3.0.0".into()));
        assert_eq!(
            release_at(
                &timeline(),
                Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()
            ),
            None
        );
    }

    #[test]
    fn vendor_join_keys_win_by_majority() {
        let signals = vec![
            signal(Some("v2.0.0"), ts(6)),
            signal(Some("v2.0.0"), ts(7)),
            signal(Some("v3.0.0"), ts(11)),
        ];
        assert_eq!(
            suspect_release(&signals, &timeline()),
            Some("v2.0.0".into())
        );
    }

    #[test]
    fn timeline_fallback_when_no_vendor_release() {
        let signals = vec![signal(None, ts(6)), signal(None, ts(12))];
        // Earliest first_seen (day 6) → v2.0.0 was live.
        assert_eq!(
            suspect_release(&signals, &timeline()),
            Some("v2.0.0".into())
        );
    }

    #[test]
    fn no_data_no_attribution() {
        assert_eq!(suspect_release(&[], &timeline()), None);
        let signals = vec![signal(
            None,
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        )];
        assert_eq!(suspect_release(&signals, &timeline()), None);
    }
}
