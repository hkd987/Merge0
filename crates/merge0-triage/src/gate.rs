//! The gate (PRD §4, P0-5): per Report, a Work Order or a SKIP with reason.
//!
//! Order of evaluation:
//! 1. **Deterministic guards** — below `min_severity`, or no evidence →
//!    SKIP without spending a model call.
//! 2. **Model evaluation** — the config prompt as system, the report bundle
//!    (summary, evidence, suspect release, prior attempts, intent excerpt)
//!    as the user turn. Expected reply: a JSON object.
//! 3. **Code-level enforcement** — a `work` decision without non-empty,
//!    testable `success_criteria` (or without `repro`) is coerced to SKIP.
//!    Fail-closed: unparseable model output is a SKIP with reason, never an
//!    emitted Work Order.

use crate::config::GateConfig;
use crate::{truncate_with_marker, Result};
use merge0_model::{extract_json_object, Model, ModelRequest};
use merge0_signal::{GateDecision, OutcomeRef, Report, WorkOrder};
use serde::Deserialize;

pub struct GateOutcome {
    pub decision: GateDecision,
    pub tokens_used: u64,
}

/// What we ask the model to return.
#[derive(Debug, Deserialize)]
struct ModelVerdict {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    repro: Option<String>,
    #[serde(default)]
    success_criteria: Option<String>,
    #[serde(default)]
    constraints: Option<String>,
    #[serde(default)]
    suspect_change: Option<String>,
}

pub async fn evaluate(
    report: &Report,
    repo: &str,
    intent_excerpt: &str,
    prior_attempts: Vec<OutcomeRef>,
    config: &GateConfig,
    model: &dyn Model,
) -> Result<GateOutcome> {
    // Deterministic guards: no model spend on obvious skips.
    if report.severity < config.min_severity {
        return Ok(GateOutcome {
            decision: GateDecision::Skip {
                reason: format!(
                    "severity {:?} below gate threshold {:?}",
                    report.severity, config.min_severity
                ),
            },
            tokens_used: 0,
        });
    }
    if report.evidence.is_empty() {
        return Ok(GateOutcome {
            decision: GateDecision::Skip {
                reason: "no evidence links — nothing a reviewer could verify".into(),
            },
            tokens_used: 0,
        });
    }

    let request = ModelRequest {
        system: config.prompt.clone(),
        prompt: build_prompt(report, intent_excerpt, &prior_attempts, config),
        max_tokens: 2048,
    };
    let response = model.complete(&request).await?;
    let decision = interpret(&response.text, report, repo, &prior_attempts, config);
    Ok(GateOutcome {
        decision,
        tokens_used: response.tokens_used,
    })
}

fn build_prompt(
    report: &Report,
    intent_excerpt: &str,
    prior_attempts: &[OutcomeRef],
    config: &GateConfig,
) -> String {
    let evidence: Vec<String> = report
        .evidence
        .iter()
        .map(|e| {
            format!(
                "- [{}] {}: {}",
                serde_json::to_string(&e.kind).unwrap(),
                e.label,
                e.url
            )
        })
        .collect();
    let prior: Vec<String> = prior_attempts
        .iter()
        .map(|p| {
            format!(
                "- {} on {} ({})",
                serde_json::to_string(&p.outcome).unwrap(),
                p.occurred_at.date_naive(),
                p.note.as_deref().unwrap_or("no note")
            )
        })
        .collect();
    format!(
        "REPORT\ntitle: {title}\nseverity: {severity:?}\naffected: {affected}\n\
         suspect_release: {release}\n\nsummary:\n{summary}\n\nevidence:\n{evidence}\n\n\
         prior attempts (outcome memory):\n{prior}\n\nintent notes (customer-authored):\n{intent}\n\n\
         Respond with a single JSON object: either\n\
         {{\"decision\":\"work\",\"summary\":...,\"repro\":...,\"success_criteria\":...,\"constraints\":...,\"suspect_change\":...}}\n\
         or {{\"decision\":\"skip\",\"reason\":...}}.",
        title = report.title,
        severity = report.severity,
        affected = report
            .affected_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into()),
        release = report.suspect_release.as_deref().unwrap_or("unknown"),
        summary = report.summary,
        evidence = if evidence.is_empty() { "- none".into() } else { evidence.join("\n") },
        prior = if prior.is_empty() { "- none".into() } else { prior.join("\n") },
        intent = truncate_with_marker(intent_excerpt, config.max_section_chars),
    )
}

/// Turn model text into a GateDecision, enforcing P0-5 in code.
fn interpret(
    text: &str,
    report: &Report,
    repo: &str,
    prior_attempts: &[OutcomeRef],
    config: &GateConfig,
) -> GateDecision {
    let Some(json) = extract_json_object(text) else {
        return GateDecision::Skip {
            reason: "gate output contained no JSON object (fail-closed)".into(),
        };
    };
    let verdict: ModelVerdict = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            return GateDecision::Skip {
                reason: format!("gate output unparseable ({e}) (fail-closed)"),
            }
        }
    };
    match verdict.decision.as_str() {
        "skip" => GateDecision::Skip {
            reason: verdict
                .reason
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "gate skipped without a reason".into()),
        },
        "work" => {
            let success_criteria = verdict.success_criteria.unwrap_or_default();
            let repro = verdict.repro.unwrap_or_default();
            if success_criteria.trim().is_empty() {
                // P0-5: no Work Order without testable success criteria.
                return GateDecision::Skip {
                    reason:
                        "gate proposed work without testable success criteria (coerced to skip)"
                            .into(),
                };
            }
            if repro.trim().is_empty() {
                return GateDecision::Skip {
                    reason: "gate proposed work without a repro (coerced to skip)".into(),
                };
            }
            GateDecision::Work {
                work_order: WorkOrder {
                    report_id: report.id,
                    repo: repo.to_string(),
                    summary: verdict
                        .summary
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| report.title.clone()),
                    evidence: report.evidence.clone(),
                    repro,
                    suspect_change: verdict.suspect_change.or_else(|| {
                        report
                            .suspect_release
                            .as_ref()
                            .map(|r| format!("regressed in {r}"))
                    }),
                    success_criteria,
                    constraints: verdict.constraints.unwrap_or_default(),
                    prior_attempts: prior_attempts.to_vec(),
                    diff_budget: config.diff_budget(),
                },
            }
        }
        other => GateDecision::Skip {
            reason: format!("gate returned unknown decision {other:?} (fail-closed)"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use merge0_model::ScriptedModel;
    use merge0_signal::{EvidenceKind, EvidenceLink, ReportKind, ReportStatus, Severity};
    use ulid::Ulid;

    fn config() -> GateConfig {
        toml::from_str(
            r#"
            prompt = "you are the gate"
            min_severity = "medium"
            max_work_orders_per_run = 3
            "#,
        )
        .unwrap()
    }

    fn report(severity: Severity, with_evidence: bool) -> Report {
        Report {
            id: Ulid::new(),
            kind: ReportKind::Maintenance,
            title: "Crash".into(),
            summary: "42 users".into(),
            severity,
            evidence: if with_evidence {
                vec![EvidenceLink {
                    kind: EvidenceKind::Issue,
                    label: "issue".into(),
                    url: "https://sentry.example.com/1".into(),
                }]
            } else {
                vec![]
            },
            signal_ids: vec![Ulid::new()],
            fingerprints: vec!["sentry:x".into()],
            suspect_release: Some("v2.3.0".into()),
            affected_count: Some(42),
            status: ReportStatus::Pending,
            created_at: Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
        }
    }

    #[tokio::test]
    async fn severity_guard_skips_without_model_call() {
        let model = ScriptedModel::new(Vec::<String>::new());
        let outcome = evaluate(
            &report(Severity::Low, true),
            "o/r",
            "",
            vec![],
            &config(),
            &model,
        )
        .await
        .unwrap();
        assert!(matches!(outcome.decision, GateDecision::Skip { .. }));
        assert_eq!(outcome.tokens_used, 0);
        assert!(model.requests().is_empty(), "no model spend on guard skips");
    }

    #[tokio::test]
    async fn missing_evidence_guard_skips() {
        let model = ScriptedModel::new(Vec::<String>::new());
        let outcome = evaluate(
            &report(Severity::High, false),
            "o/r",
            "",
            vec![],
            &config(),
            &model,
        )
        .await
        .unwrap();
        match outcome.decision {
            GateDecision::Skip { reason } => assert!(reason.contains("no evidence")),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn work_decision_builds_a_complete_work_order() {
        let model = ScriptedModel::new([r#"Decision follows.
            {"decision":"work","summary":"Fix null district","repro":"open /districts/sync",
             "success_criteria":"regression test passes","constraints":"don't touch scheduler"}"#]);
        let r = report(Severity::High, true);
        let outcome = evaluate(&r, "chalk/chalk", "intent text", vec![], &config(), &model)
            .await
            .unwrap();
        let GateDecision::Work { work_order } = outcome.decision else {
            panic!("expected work");
        };
        assert_eq!(work_order.report_id, r.id);
        assert_eq!(work_order.repo, "chalk/chalk");
        assert_eq!(work_order.success_criteria, "regression test passes");
        assert_eq!(
            work_order.suspect_change.as_deref(),
            Some("regressed in v2.3.0")
        );
        assert_eq!(work_order.diff_budget, config().diff_budget());
        assert_eq!(outcome.tokens_used, 1000);
        // Prompt carried the report bundle.
        let prompt = &model.requests()[0].prompt;
        assert!(prompt.contains("suspect_release: v2.3.0"));
        assert!(prompt.contains("intent text"));
    }

    #[tokio::test]
    async fn work_without_success_criteria_is_coerced_to_skip() {
        // P0-5 enforced in code, not prompt discipline.
        let model = ScriptedModel::new([
            r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"  "}"#,
        ]);
        let outcome = evaluate(
            &report(Severity::High, true),
            "o/r",
            "",
            vec![],
            &config(),
            &model,
        )
        .await
        .unwrap();
        match outcome.decision {
            GateDecision::Skip { reason } => assert!(reason.contains("success criteria")),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_output_fails_closed() {
        for bad in [
            "total garbage",
            r#"{"decision":"maybe"}"#,
            r#"{"decision":42}"#,
        ] {
            let model = ScriptedModel::new([bad]);
            let outcome = evaluate(
                &report(Severity::High, true),
                "o/r",
                "",
                vec![],
                &config(),
                &model,
            )
            .await
            .unwrap();
            assert!(
                matches!(outcome.decision, GateDecision::Skip { .. }),
                "must fail closed for {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn skip_reason_is_preserved_and_prior_attempts_ride_into_the_order() {
        let model = ScriptedModel::new([
            r#"{"decision":"skip","reason":"intended behavior per MERGE0.md"}"#,
            r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c"}"#,
        ]);
        let outcome = evaluate(
            &report(Severity::High, true),
            "o/r",
            "",
            vec![],
            &config(),
            &model,
        )
        .await
        .unwrap();
        match outcome.decision {
            GateDecision::Skip { reason } => assert_eq!(reason, "intended behavior per MERGE0.md"),
            other => panic!("expected skip, got {other:?}"),
        }

        let prior = vec![OutcomeRef {
            work_order_id: Ulid::new(),
            outcome: merge0_signal::OutcomeKind::Reverted,
            occurred_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            note: Some("broke admin view".into()),
        }];
        let outcome = evaluate(
            &report(Severity::High, true),
            "o/r",
            "",
            prior.clone(),
            &config(),
            &model,
        )
        .await
        .unwrap();
        let GateDecision::Work { work_order } = outcome.decision else {
            panic!("expected work");
        };
        assert_eq!(work_order.prior_attempts, prior);
        // The prompt surfaced the revert to the model.
        assert!(model.requests()[1].prompt.contains("broke admin view"));
    }
}
