//! Scenario corpus: curated Signal sets with expected gate outcomes,
//! loaded from `evals/scenarios/*.toml`. Signals are specified in a
//! compact eval-friendly shape and expanded into real `Signal`s; the
//! Report is then built through the REAL clustering/assembly path so the
//! gate sees exactly what production would show it.

use chrono::{DateTime, Duration, Utc};
use merge0_signal::{
    EvidenceKind, EvidenceLink, JoinKeys, OutcomeKind, OutcomeRef, Report, ReportKind, Severity,
    Signal, SignalKind, Source,
};
use merge0_triage::cluster::{assemble_report, classify, cluster_signals};
use merge0_triage::config::GateConfig;
use serde::Deserialize;
use std::path::Path;
use ulid::Ulid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    /// Customer-authored intent prose handed to the gate.
    #[serde(default)]
    pub intent: String,
    /// Marks member fingerprints as previously dismissed "intended
    /// behavior" (drives the Opportunity classification path).
    #[serde(default)]
    pub intended_history: bool,
    #[serde(default)]
    pub suspect_release: Option<String>,
    pub signals: Vec<SignalSpec>,
    #[serde(default)]
    pub prior_attempts: Vec<PriorSpec>,
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalSpec {
    pub source: Source,
    pub kind: SignalKind,
    pub severity: Severity,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub affected_count: Option<u64>,
    /// Correlation keys (same stack_hash across specs clusters them).
    #[serde(default)]
    pub stack_hash: Option<String>,
    #[serde(default)]
    pub url_path: Option<String>,
    #[serde(default)]
    pub release: Option<String>,
    /// Distinct fingerprint per spec unless set explicitly.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// When true the signal carries no evidence link (exercises the
    /// gate's deterministic no-evidence guard).
    #[serde(default)]
    pub no_evidence: bool,
    #[serde(default = "default_evidence_label")]
    pub evidence_label: String,
    #[serde(default = "default_evidence_url")]
    pub evidence_url: String,
    #[serde(default = "default_hours_ago")]
    pub first_seen_hours_ago: i64,
    #[serde(default = "default_one")]
    pub last_seen_hours_ago: i64,
}

fn default_evidence_label() -> String {
    "source issue".into()
}
fn default_evidence_url() -> String {
    "https://tool.example.com/issues/1".into()
}
fn default_hours_ago() -> i64 {
    30
}
fn default_one() -> i64 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorSpec {
    pub outcome: OutcomeKind,
    pub days_ago: i64,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// "work" | "skip" | "opportunity" (opportunity = classified away from
    /// the gate entirely — deterministic, no model call).
    pub decision: String,
    /// When false, the deterministic guards must answer (tokens_used == 0).
    #[serde(default = "default_true")]
    pub model_called: bool,
    /// Substrings (case-insensitive) that must appear in the work order
    /// (summary + repro + success_criteria + constraints + suspect_change).
    #[serde(default)]
    pub work_order_mentions: Vec<String>,
    /// Substrings (case-insensitive) that must appear in the skip reason.
    #[serde(default)]
    pub skip_reason_mentions: Vec<String>,
    /// Strings that must NOT appear anywhere in a produced work order
    /// (secret canaries).
    #[serde(default)]
    pub forbidden: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Scenario {
    pub fn build_signals(&self, now: DateTime<Utc>) -> Vec<Signal> {
        self.signals
            .iter()
            .enumerate()
            .map(|(index, spec)| Signal {
                id: Ulid::new(),
                source: spec.source,
                source_ref: format!("eval-{index}"),
                kind: spec.kind,
                severity: spec.severity,
                title: spec.title.clone(),
                body: spec.body.clone(),
                evidence: if spec.no_evidence {
                    vec![]
                } else {
                    vec![EvidenceLink {
                        kind: EvidenceKind::Issue,
                        label: spec.evidence_label.clone(),
                        url: spec.evidence_url.clone(),
                    }]
                },
                fingerprint: spec
                    .fingerprint
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}:{index}", spec.source.as_str(), self.name)),
                join_keys: JoinKeys {
                    release: spec.release.clone(),
                    stack_hash: spec.stack_hash.clone(),
                    account_id: None,
                    url_path: spec.url_path.clone(),
                },
                affected_count: spec.affected_count,
                first_seen: now - Duration::hours(spec.first_seen_hours_ago),
                last_seen: now - Duration::hours(spec.last_seen_hours_ago),
                raw: serde_json::Value::Null,
            })
            .collect()
    }

    /// Build the Report exactly as production would: cluster → classify →
    /// assemble under the gate config's evidence budget. Returns the
    /// classification too, so "opportunity" expectations are checkable
    /// without a model call. Multi-cluster scenarios take the largest
    /// cluster (corpus convention: one defect per scenario).
    pub fn build_report(
        &self,
        config: &GateConfig,
        now: DateTime<Utc>,
    ) -> (Report, ReportKind, Vec<OutcomeRef>) {
        let signals = self.build_signals(now);
        let clusters = cluster_signals(signals);
        let cluster = clusters
            .into_iter()
            .max_by_key(|c| c.signals.len())
            .expect("scenario has at least one signal");
        let kind = classify(&cluster.signals, self.intended_history);
        let mut report = assemble_report(&cluster, kind, self.suspect_release.clone(), config, now);
        // Scenarios may pin the release directly (no timeline table here).
        report.suspect_release = self.suspect_release.clone();
        let priors = self
            .prior_attempts
            .iter()
            .map(|p| OutcomeRef {
                work_order_id: Ulid::new(),
                outcome: p.outcome,
                occurred_at: now - Duration::days(p.days_ago),
                note: p.note.clone(),
            })
            .collect();
        (report, kind, priors)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid scenario {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("scenario {0}: {1}")]
    Invalid(String, String),
}

/// Load every `*.toml` scenario in a directory, sorted by file name.
pub fn load_scenarios(dir: &Path) -> Result<Vec<Scenario>, ScenarioError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ScenarioError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    let mut scenarios = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path).map_err(|source| ScenarioError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let scenario: Scenario = toml::from_str(&text).map_err(|source| ScenarioError::Parse {
            path: path.display().to_string(),
            source: Box::new(source),
        })?;
        validate(&scenario)?;
        scenarios.push(scenario);
    }
    Ok(scenarios)
}

fn validate(scenario: &Scenario) -> Result<(), ScenarioError> {
    let invalid = |why: &str| {
        Err(ScenarioError::Invalid(
            scenario.name.clone(),
            why.to_string(),
        ))
    };
    if scenario.signals.is_empty() {
        return invalid("needs at least one signal");
    }
    match scenario.expect.decision.as_str() {
        "work" | "skip" | "opportunity" => {}
        other => return invalid(&format!("unknown expected decision {other:?}")),
    }
    if scenario.expect.decision == "work" && !scenario.expect.skip_reason_mentions.is_empty() {
        return invalid("skip_reason_mentions on a work expectation");
    }
    if scenario.expect.decision != "work" && !scenario.expect.work_order_mentions.is_empty() {
        return invalid("work_order_mentions on a non-work expectation");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_config() -> GateConfig {
        toml::from_str(
            r#"
            prompt = "you are the gate"
            min_severity = "medium"
            max_work_orders_per_run = 3
            "#,
        )
        .unwrap()
    }

    fn scenario(toml_text: &str) -> Scenario {
        toml::from_str(toml_text).unwrap()
    }

    const CROSS_SOURCE: &str = r#"
        name = "cross-source"
        description = "same defect, two tools"
        intent = "schools may exist without districts"
        suspect_release = "v2.3.0"

        [[signals]]
        source = "sentry"
        kind = "exception"
        severity = "high"
        title = "TypeError: districtId undefined"
        stack_hash = "abc123"
        affected_count = 33

        [[signals]]
        source = "posthog"
        kind = "ux_friction"
        severity = "medium"
        title = "Rage clicks on /districts/sync"
        stack_hash = "abc123"
        affected_count = 21

        [[prior_attempts]]
        outcome = "reverted"
        days_ago = 20
        note = "broke admin view"

        [expect]
        decision = "work"
        work_order_mentions = ["district"]
        forbidden = ["FAKE-CANARY"]
    "#;

    #[test]
    fn signals_expand_with_correlation_keys_and_windows() {
        let s = scenario(CROSS_SOURCE);
        let now = Utc::now();
        let signals = s.build_signals(now);
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].join_keys.stack_hash.as_deref(), Some("abc123"));
        assert!(signals[0].first_seen < signals[0].last_seen);
        assert_ne!(
            signals[0].fingerprint, signals[1].fingerprint,
            "distinct fingerprints unless pinned"
        );
    }

    #[test]
    fn report_builds_through_the_real_cluster_path() {
        let s = scenario(CROSS_SOURCE);
        let (report, kind, priors) = s.build_report(&gate_config(), Utc::now());
        assert_eq!(kind, ReportKind::Maintenance);
        assert_eq!(report.signal_ids.len(), 2, "both signals, one report");
        assert_eq!(report.severity, Severity::High, "max severity wins");
        assert_eq!(report.suspect_release.as_deref(), Some("v2.3.0"));
        assert_eq!(priors.len(), 1);
        assert_eq!(priors[0].outcome, OutcomeKind::Reverted);
    }

    #[test]
    fn the_shipped_corpus_loads_and_builds() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals/scenarios");
        let scenarios = load_scenarios(&dir).expect("shipped scenarios must parse");
        assert!(scenarios.len() >= 12, "corpus stays substantial");
        let config = gate_config();
        for scenario in &scenarios {
            // Every scenario must survive report assembly.
            let (report, kind, _) = scenario.build_report(&config, Utc::now());
            assert!(!report.title.is_empty(), "{}", scenario.name);
            if scenario.expect.decision == "opportunity" {
                assert_eq!(
                    kind,
                    ReportKind::Opportunity,
                    "{}: expected the classifier to route this away from the gate",
                    scenario.name
                );
            }
        }
    }

    #[test]
    fn expectation_shape_is_validated() {
        let bad = scenario(
            r#"
            name = "bad"
            description = "d"
            [[signals]]
            source = "sentry"
            kind = "exception"
            severity = "high"
            title = "t"
            [expect]
            decision = "maybe"
            "#,
        );
        assert!(validate(&bad).is_err());
    }
}
