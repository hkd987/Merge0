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
use crate::Result;
use merge0_model::{extract_json_object, Model, ModelRequest};
use merge0_signal::{GateConfidence, GateDecision, OutcomeRef, Report, WorkOrder};
use serde::Deserialize;

pub struct GateOutcome {
    pub decision: GateDecision,
    pub tokens_used: u64,
    /// The exact system+user context the gate saw when it decided —
    /// persisted so "why did it decide that?" is answerable by replay
    /// rather than inference. Deterministic-guard skips record the guard
    /// instead (no model call happened).
    pub context: String,
}

/// What we ask the model to return.
///
/// Every text field tolerates a JSON array of strings as well as a plain
/// string — real models routinely emit `"success_criteria": ["a", "b"]`,
/// and rejecting that would collapse every work decision to a fail-closed
/// skip (found by the gate eval, not by any scripted test).
#[derive(Debug, Deserialize)]
struct ModelVerdict {
    decision: String,
    #[serde(default)]
    reason: Option<Text>,
    #[serde(default)]
    summary: Option<Text>,
    #[serde(default)]
    repro: Option<Text>,
    #[serde(default)]
    success_criteria: Option<Text>,
    #[serde(default)]
    constraints: Option<Text>,
    #[serde(default)]
    suspect_change: Option<Text>,
    #[serde(default)]
    confidence: Option<Text>,
}

/// A string, or a list of strings joined into one.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Text {
    One(String),
    Many(Vec<String>),
}

impl Text {
    fn into_string(self) -> String {
        match self {
            Text::One(text) => text,
            Text::Many(items) => items.join("; "),
        }
    }
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
            context: "deterministic guard: severity below threshold (no model call)".into(),
        });
    }
    if report.evidence.is_empty() {
        return Ok(GateOutcome {
            decision: GateDecision::Skip {
                reason: "no evidence links — nothing a reviewer could verify".into(),
            },
            tokens_used: 0,
            context: "deterministic guard: no evidence links (no model call)".into(),
        });
    }

    let request = ModelRequest {
        system: config.prompt.clone(),
        prompt: build_prompt(report, intent_excerpt, &prior_attempts, config),
        max_tokens: 2048,
    };
    let response = model.complete(&request).await?;
    let mut tokens_used = response.tokens_used;
    let mut decision = interpret(&response.text, report, repo, &prior_attempts, config);
    // One retry when the output itself was malformed (no JSON object, a
    // bad escape): that is sampling noise, not judgment — the eval canary
    // caught a live flake where a single invalid escape fail-closed a
    // clear defect to SKIP. Judgment-level skips never retry, and a
    // malformed retry still fails closed.
    if malformed_output(&decision) {
        let retry = model.complete(&request).await?;
        tokens_used += retry.tokens_used;
        decision = interpret(&retry.text, report, repo, &prior_attempts, config);
    }
    let context = format!(
        "=== SYSTEM ===\n{}\n\n=== PROMPT ===\n{}",
        request.system, request.prompt
    );
    Ok(GateOutcome {
        decision,
        tokens_used,
        context,
    })
}

/// Malformed *output* (as opposed to a parseable verdict we disagree
/// with): both fail-closed reasons that `interpret` derives from the raw
/// text rather than from a decision.
fn malformed_output(decision: &GateDecision) -> bool {
    matches!(decision, GateDecision::Skip { reason } if reason.starts_with("gate output"))
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
    // Outcome memory without age is a blunt instrument: an eight-month-old
    // revert should inform, not veto. Wall-clock is read here rather than
    // threaded through `evaluate` because the marker is day-coarse.
    let now = chrono::Utc::now();
    let prior: Vec<String> = prior_attempts
        .iter()
        .map(|p| {
            let age_days = (now - p.occurred_at).num_days().max(0);
            let staleness = if age_days > config.stale_prior_days as i64 {
                " — STALE"
            } else {
                ""
            };
            // The PR link is what makes a revert actionable rather than
            // merely discouraging: it is the only way the gate can say
            // "don't repeat what that attempt did" instead of declining.
            let pr = match p.pr_url.as_deref() {
                Some(url) => format!(" [attempt PR: {url}]"),
                None => String::new(),
            };
            format!(
                "- {} on {} ({age_days} days ago{staleness}) ({}){pr}",
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
         {{\"decision\":\"work\",\"summary\":...,\"repro\":...,\"success_criteria\":...,\"constraints\":...,\"suspect_change\":...,\"confidence\":\"high|medium|low\"}}\n\
         or {{\"decision\":\"skip\",\"reason\":...}}. Every field is a plain string.",
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
        intent = merge0_context::intent::relevant_intent(
            intent_excerpt,
            &format!("{} {}", report.title, report.summary),
            config.max_section_chars,
        ),
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
                .map(Text::into_string)
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "gate skipped without a reason".into()),
        },
        "work" => {
            let success_criteria = verdict
                .success_criteria
                .map(Text::into_string)
                .unwrap_or_default();
            let repro = verdict.repro.map(Text::into_string).unwrap_or_default();
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
                        .map(Text::into_string)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| report.title.clone()),
                    evidence: report.evidence.clone(),
                    repro,
                    suspect_change: verdict.suspect_change.map(Text::into_string).or_else(|| {
                        report
                            .suspect_release
                            .as_ref()
                            .map(|r| format!("regressed in {r}"))
                    }),
                    success_criteria,
                    constraints: verdict
                        .constraints
                        .map(Text::into_string)
                        .unwrap_or_default(),
                    prior_attempts: prior_attempts.to_vec(),
                    diff_budget: config.diff_budget(),
                    // Absent or garbage confidence parses to Low — a model
                    // that can't state its confidence never auto-dispatches.
                    confidence: verdict
                        .confidence
                        .map(Text::into_string)
                        .map(|c| GateConfidence::parse_lenient(&c))
                        .unwrap_or_default(),
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
             "success_criteria":"regression test passes","constraints":"don't touch scheduler",
             "confidence":"high"}"#]);
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
        assert_eq!(work_order.confidence, GateConfidence::High);
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
    async fn list_valued_fields_are_joined_not_rejected() {
        // Found by the live gate eval: real models emit arrays for the
        // text fields; that must not collapse a work decision to a skip.
        let model = ScriptedModel::new([r#"{"decision":"work","summary":"Fix crash",
                "repro":["open /districts/sync","observe the panic"],
                "success_criteria":["unassigned schools render","regression test passes"],
                "constraints":["keep it minimal"]}"#]);
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
        let GateDecision::Work { work_order } = outcome.decision else {
            panic!("expected work");
        };
        assert_eq!(work_order.repro, "open /districts/sync; observe the panic");
        assert_eq!(
            work_order.success_criteria,
            "unassigned schools render; regression test passes"
        );
        assert_eq!(work_order.constraints, "keep it minimal");
    }

    #[tokio::test]
    async fn confidence_parses_leniently_and_defaults_low() {
        // (scripted verdict, expected confidence): absence, garbage, and
        // list-shaped values must all land on Low — never on High by accident.
        for (verdict, expected) in [
            (
                r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c"}"#,
                GateConfidence::Low,
            ),
            (
                r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c","confidence":"HIGH"}"#,
                GateConfidence::High,
            ),
            (
                r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c","confidence":" Medium "}"#,
                GateConfidence::Medium,
            ),
            (
                r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c","confidence":"absolutely"}"#,
                GateConfidence::Low,
            ),
            (
                r#"{"decision":"work","summary":"s","repro":"r","success_criteria":"c","confidence":["high","medium"]}"#,
                GateConfidence::Low,
            ),
        ] {
            let model = ScriptedModel::new([verdict]);
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
            let GateDecision::Work { work_order } = outcome.decision else {
                panic!("expected work for {verdict}");
            };
            assert_eq!(work_order.confidence, expected, "for {verdict}");
        }
    }

    #[tokio::test]
    async fn unparseable_output_fails_closed() {
        // Two copies scripted: malformed output earns exactly one retry,
        // and a retry that is malformed again must still fail closed.
        for bad in [
            "total garbage",
            r#"{"decision":"maybe"}"#,
            r#"{"decision":42}"#,
        ] {
            let model = ScriptedModel::new([bad, bad]);
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

    /// The canary-caught flake: one sample with an invalid escape must not
    /// silently skip a clear defect. A single retry recovers; tokens from
    /// both calls are accounted.
    #[tokio::test]
    async fn malformed_output_retries_once_and_recovers() {
        let model = ScriptedModel::new([
            r#"{"decision":"work","summary":"broken \escape"#,
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
        assert!(
            matches!(outcome.decision, GateDecision::Work { .. }),
            "retry must recover the verdict: {:?}",
            outcome.decision
        );
        assert_eq!(model.requests().len(), 2, "exactly one retry");

        // A judgment-level skip is NOT malformed output: no retry, even
        // with a tempting work verdict scripted behind it.
        let model = ScriptedModel::new([
            r#"{"decision":"skip","reason":"intended behavior"}"#,
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
        assert!(matches!(outcome.decision, GateDecision::Skip { .. }));
        assert_eq!(model.requests().len(), 1, "judgment skips never retry");
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
            pr_url: Some("https://github.com/o/r/pull/412".into()),
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
        // The prompt surfaced the revert to the model — and, since v0.5, the
        // PR it produced. Without that link the gate can only decline; with
        // it, it can instruct the agent to take a different approach.
        assert!(model.requests()[1].prompt.contains("broke admin view"));
        assert!(
            model.requests()[1]
                .prompt
                .contains("https://github.com/o/r/pull/412"),
            "the attempt's PR must reach the gate: {}",
            model.requests()[1].prompt
        );
    }

    /// Memory without decay over-vetoes. A revert from last week is a real
    /// signal about today's codebase; one from three years ago is a signal
    /// about a codebase that no longer exists, and the gate is told which
    /// it is looking at rather than treating both as equally damning.
    #[tokio::test]
    async fn prior_attempts_are_rendered_with_age_and_a_stale_marker() {
        let model = ScriptedModel::new(vec![
            r#"{"decision":"skip","reason":"no"}"#.to_string(),
            r#"{"decision":"skip","reason":"no"}"#.to_string(),
        ]);
        let now = Utc::now();
        let recent = vec![OutcomeRef {
            work_order_id: Ulid::new(),
            outcome: merge0_signal::OutcomeKind::Reverted,
            occurred_at: now - chrono::Duration::days(3),
            note: Some("recent revert".into()),
            pr_url: None,
        }];
        let ancient = vec![OutcomeRef {
            work_order_id: Ulid::new(),
            outcome: merge0_signal::OutcomeKind::Reverted,
            occurred_at: now - chrono::Duration::days(400),
            note: Some("ancient revert".into()),
            pr_url: None,
        }];
        for prior in [recent, ancient] {
            evaluate(
                &report(Severity::High, true),
                "o/r",
                "",
                prior,
                &config(),
                &model,
            )
            .await
            .unwrap();
        }
        let fresh = &model.requests()[0].prompt;
        assert!(fresh.contains("3 days ago"), "{fresh}");
        assert!(
            !fresh.contains("STALE"),
            "recent memory is not stale: {fresh}"
        );
        let old = &model.requests()[1].prompt;
        assert!(old.contains("400 days ago"), "{old}");
        assert!(old.contains("STALE"), "aged memory is marked: {old}");
    }

    /// The disconnection this retrieval layer exists to fix: `merge0-hardening`
    /// writes earned constraints into MERGE0.md's machine fence, and the gate
    /// used to see neither the fence (stripped) nor the tail of a long doc
    /// (blind-truncated). Both paths are asserted here, at the seam that
    /// actually builds the prompt.
    #[tokio::test]
    async fn intent_reaching_the_model_keeps_the_machine_fence_and_discloses_drops() {
        let model = ScriptedModel::new(vec![r#"{"decision":"skip","reason":"no"}"#.to_string()]);
        let filler = "Unrelated onboarding prose. ".repeat(120);
        let intent = format!(
            "# MERGE0.md\n\n## Onboarding\n{filler}\n\n## Invariants\n             - Schools may exist without a district.\n\n             <!-- merge0:managed:start -->\n             - do not retry the district backfill job\n             <!-- merge0:managed:end -->\n"
        );
        evaluate(
            &report(Severity::High, true),
            "o/r",
            &intent,
            vec![],
            &config(),
            &model,
        )
        .await
        .unwrap();
        let prompt = &model.requests()[0].prompt;
        assert!(
            prompt.contains("district backfill job"),
            "hardening amendments must reach the gate: {prompt}"
        );
        assert!(
            prompt.contains("[NOTE:") && prompt.contains("\"Onboarding\""),
            "dropped intent is disclosed, never silent: {prompt}"
        );
    }
}
