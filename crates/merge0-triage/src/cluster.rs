//! Deterministic clustering: correlated Signals → one Report (PRD P0-3).
//!
//! Correlation key preference, per signal:
//! 1. `join_keys.stack_hash` — the cross-source join (a Sentry exception and
//!    a PostHog error for the same defect class share it by construction).
//! 2. `join_keys.url_path` — location correlation for UX friction.
//! 3. `fingerprint` — everything else stays a singleton cluster.
//!
//! Report kind classification (Opportunity boundary, PRD P2 "Opportunity
//! Reports"):
//! 1. Any member `exception`/`regression` → **Maintenance**.
//! 2. Else, any member fingerprint previously dismissed as
//!    `intended_behavior` → **Opportunity** (users repeatedly colliding with
//!    the design).
//! 3. Else, all members `ux_friction` with no stack hash → **Opportunity**
//!    (friction with no error underneath: a missing affordance, not a bug).
//! 4. Else → **Maintenance**.

use crate::config::GateConfig;
use crate::truncate_with_marker;
use chrono::{DateTime, Utc};
use merge0_signal::{EvidenceLink, Report, ReportKind, ReportStatus, Severity, Signal, SignalKind};
use std::collections::BTreeMap;
use ulid::Ulid;

/// A cluster of correlated signals, pre-Report.
#[derive(Debug)]
pub struct Cluster {
    pub key: String,
    pub signals: Vec<Signal>,
}

/// Group signals by correlation key. Deterministic: clusters ordered by key,
/// members keep input order.
pub fn cluster_signals(signals: Vec<Signal>) -> Vec<Cluster> {
    let mut groups: BTreeMap<String, Vec<Signal>> = BTreeMap::new();
    for signal in signals {
        let key = correlation_key(&signal);
        groups.entry(key).or_default().push(signal);
    }
    groups
        .into_iter()
        .map(|(key, signals)| Cluster { key, signals })
        .collect()
}

fn correlation_key(signal: &Signal) -> String {
    if let Some(hash) = signal.join_keys.stack_hash.as_deref() {
        format!("stack:{hash}")
    } else if let Some(path) = signal.join_keys.url_path.as_deref() {
        format!("path:{path}")
    } else {
        format!("fp:{}", signal.fingerprint)
    }
}

/// Classify a cluster (see module docs). `intended_history` reports whether
/// any member fingerprint was previously dismissed as intended behavior.
pub fn classify(signals: &[Signal], intended_history: bool) -> ReportKind {
    let any_defect = signals
        .iter()
        .any(|s| matches!(s.kind, SignalKind::Exception | SignalKind::Regression));
    if any_defect {
        return ReportKind::Maintenance;
    }
    if intended_history {
        return ReportKind::Opportunity;
    }
    let all_frictionless_ux = signals
        .iter()
        .all(|s| s.kind == SignalKind::UxFriction && s.join_keys.stack_hash.is_none());
    if all_frictionless_ux {
        return ReportKind::Opportunity;
    }
    ReportKind::Maintenance
}

/// Assemble a Report from a cluster under the evidence budget.
pub fn assemble_report(
    cluster: &Cluster,
    kind: ReportKind,
    suspect_release: Option<String>,
    config: &GateConfig,
    now: DateTime<Utc>,
) -> Report {
    let severity = cluster
        .signals
        .iter()
        .map(|s| s.severity)
        .max()
        .unwrap_or(Severity::Low);
    // Affected counts across sources may overlap; the sum is an upper bound
    // and is labeled as such in the summary.
    let affected: u64 = cluster
        .signals
        .iter()
        .filter_map(|s| s.affected_count)
        .sum();
    let title = cluster
        .signals
        .iter()
        .max_by_key(|s| (s.severity, s.affected_count.unwrap_or(0)))
        .map(|s| s.title.clone())
        .unwrap_or_else(|| cluster.key.clone());

    let sources: Vec<&str> = {
        let mut seen = Vec::new();
        for signal in &cluster.signals {
            let name = signal.source.as_str();
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
        seen
    };
    let first_seen = cluster.signals.iter().map(|s| s.first_seen).min();
    let last_seen = cluster.signals.iter().map(|s| s.last_seen).max();
    let mut summary = format!(
        "{} signal(s) from {} correlated on {}.",
        cluster.signals.len(),
        sources.join(" + "),
        cluster.key,
    );
    if affected > 0 {
        summary.push_str(&format!(" Up to {affected} users/accounts affected."));
    }
    if let (Some(first), Some(last)) = (first_seen, last_seen) {
        summary.push_str(&format!(" Seen {first} → {last}."));
    }
    if let Some(release) = &suspect_release {
        summary.push_str(&format!(" Suspect release: {release}."));
    }
    for signal in &cluster.signals {
        if !signal.body.is_empty() {
            summary.push_str(&format!("\n[{}] {}", signal.source.as_str(), signal.body));
        }
    }

    Report {
        id: Ulid::new(),
        kind,
        title,
        summary: truncate_with_marker(&summary, config.max_section_chars),
        severity,
        evidence: assemble_evidence(&cluster.signals, config.max_evidence_items),
        signal_ids: cluster.signals.iter().map(|s| s.id).collect(),
        fingerprints: cluster
            .signals
            .iter()
            .map(|s| s.fingerprint.clone())
            .collect(),
        suspect_release,
        affected_count: (affected > 0).then_some(affected),
        status: ReportStatus::Pending,
        created_at: now,
    }
}

/// Evidence assembly under budget: highest-severity, most-recent signals
/// first; dedupe by URL; hard cap. Deep links back to the source remain the
/// escape hatch for anything cut.
pub fn assemble_evidence(signals: &[Signal], max_items: usize) -> Vec<EvidenceLink> {
    let mut ordered: Vec<&Signal> = signals.iter().collect();
    ordered.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.last_seen.cmp(&a.last_seen))
    });
    let mut seen_urls = std::collections::HashSet::new();
    let mut evidence = Vec::new();
    for signal in ordered {
        for link in &signal.evidence {
            if evidence.len() >= max_items {
                return evidence;
            }
            if seen_urls.insert(link.url.clone()) {
                evidence.push(link.clone());
            }
        }
    }
    evidence
}

/// The handoff brief for an Opportunity Report (PRD P2): evidence in, human
/// decision out — never a Work Order.
pub fn handoff_brief(report: &Report, signals: &[Signal]) -> String {
    let mut brief = format!(
        "OPPORTUNITY BRIEF — {title}\n\n{summary}\n\nWhat users are doing:\n",
        title = report.title,
        summary = report.summary,
    );
    for signal in signals {
        brief.push_str(&format!(
            "- [{}] {} (affected: {})\n",
            signal.source.as_str(),
            signal.title,
            signal
                .affected_count
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".into()),
        ));
    }
    brief.push_str(
        "\nThis report is demand evidence, not a defect. Merge0 will not generate \
         a PR for it; take it to your spec process or an interactive agent session.",
    );
    brief
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use merge0_signal::{EvidenceKind, JoinKeys, Source};

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn signal(
        source: Source,
        kind: SignalKind,
        fp: &str,
        stack_hash: Option<&str>,
        url_path: Option<&str>,
    ) -> Signal {
        Signal {
            id: Ulid::new(),
            source,
            source_ref: fp.into(),
            kind,
            severity: Severity::High,
            title: format!("signal {fp}"),
            body: String::new(),
            evidence: vec![EvidenceLink {
                kind: EvidenceKind::Issue,
                label: format!("link {fp}"),
                url: format!("https://example.com/{fp}"),
            }],
            fingerprint: format!("{}:{fp}", source.as_str()),
            join_keys: JoinKeys {
                stack_hash: stack_hash.map(String::from),
                url_path: url_path.map(String::from),
                ..Default::default()
            },
            affected_count: Some(10),
            first_seen: ts(1),
            last_seen: ts(2),
            raw: serde_json::Value::Null,
        }
    }

    fn config() -> GateConfig {
        toml::from_str(
            r#"
            prompt = "gate"
            min_severity = "medium"
            max_work_orders_per_run = 3
            "#,
        )
        .unwrap()
    }

    #[test]
    fn cross_source_signals_with_same_stack_hash_form_one_cluster() {
        // P0-3: same defect via Sentry and PostHog → exactly one Report.
        let clusters = cluster_signals(vec![
            signal(Source::Sentry, SignalKind::Exception, "a", Some("h1"), None),
            signal(
                Source::Posthog,
                SignalKind::Exception,
                "b",
                Some("h1"),
                None,
            ),
            signal(Source::Sentry, SignalKind::Exception, "c", Some("h2"), None),
        ]);
        assert_eq!(clusters.len(), 2);
        let joint = clusters.iter().find(|c| c.key == "stack:h1").unwrap();
        assert_eq!(joint.signals.len(), 2);
        let report = assemble_report(joint, ReportKind::Maintenance, None, &config(), ts(3));
        assert_eq!(report.signal_ids.len(), 2);
        assert_eq!(report.fingerprints.len(), 2);
        assert!(report.summary.contains("sentry + posthog"));
    }

    #[test]
    fn path_correlation_when_no_stack_hash_then_fingerprint_fallback() {
        let clusters = cluster_signals(vec![
            signal(
                Source::Posthog,
                SignalKind::UxFriction,
                "r1",
                None,
                Some("/sync"),
            ),
            signal(
                Source::Zendesk,
                SignalKind::Ticket,
                "t1",
                None,
                Some("/sync"),
            ),
            signal(Source::Zendesk, SignalKind::Ticket, "t2", None, None),
        ]);
        assert_eq!(clusters.len(), 2);
        assert!(clusters
            .iter()
            .any(|c| c.key == "path:/sync" && c.signals.len() == 2));
        assert!(clusters.iter().any(|c| c.key.starts_with("fp:")));
    }

    #[test]
    fn classification_rules() {
        let exc = signal(Source::Sentry, SignalKind::Exception, "e", Some("h"), None);
        let rage = signal(
            Source::Posthog,
            SignalKind::UxFriction,
            "r",
            None,
            Some("/x"),
        );
        let ticket = signal(Source::Zendesk, SignalKind::Ticket, "t", None, Some("/x"));

        // 1. Exception present → maintenance regardless of history.
        assert_eq!(
            classify(&[exc.clone(), rage.clone()], true),
            ReportKind::Maintenance
        );
        // 2. Intended-behavior history → opportunity.
        assert_eq!(
            classify(std::slice::from_ref(&ticket), true),
            ReportKind::Opportunity
        );
        // 3. Pure UX friction with no stack hash → opportunity.
        assert_eq!(
            classify(std::slice::from_ref(&rage), false),
            ReportKind::Opportunity
        );
        // 4. Otherwise (e.g. tickets with no history) → maintenance.
        assert_eq!(classify(&[ticket], false), ReportKind::Maintenance);
    }

    #[test]
    fn evidence_budget_caps_and_dedupes() {
        let mut signals = Vec::new();
        for i in 0..10 {
            let mut s = signal(
                Source::Sentry,
                SignalKind::Exception,
                &format!("s{i}"),
                Some("h"),
                None,
            );
            // Two signals share a URL to exercise dedupe.
            if i == 1 {
                s.evidence[0].url = "https://example.com/s0".into();
            }
            signals.push(s);
        }
        let evidence = assemble_evidence(&signals, 4);
        assert_eq!(evidence.len(), 4);
        let urls: std::collections::HashSet<_> = evidence.iter().map(|e| &e.url).collect();
        assert_eq!(urls.len(), 4, "no duplicate URLs under budget");
    }

    #[test]
    fn summary_respects_section_budget() {
        let mut cluster = Cluster {
            key: "stack:h".into(),
            signals: vec![signal(
                Source::Sentry,
                SignalKind::Exception,
                "a",
                Some("h"),
                None,
            )],
        };
        cluster.signals[0].body = "b".repeat(5000);
        let report = assemble_report(&cluster, ReportKind::Maintenance, None, &config(), ts(3));
        assert!(report.summary.chars().count() < 2200);
        assert!(report.summary.contains("truncated by evidence budget"));
    }

    #[test]
    fn handoff_brief_names_the_boundary() {
        let rage = signal(
            Source::Posthog,
            SignalKind::UxFriction,
            "r",
            None,
            Some("/x"),
        );
        let report = assemble_report(
            &Cluster {
                key: "path:/x".into(),
                signals: vec![rage.clone()],
            },
            ReportKind::Opportunity,
            None,
            &config(),
            ts(3),
        );
        let brief = handoff_brief(&report, &[rage]);
        assert!(brief.contains("OPPORTUNITY BRIEF"));
        assert!(brief.contains("will not generate"));
    }
}
