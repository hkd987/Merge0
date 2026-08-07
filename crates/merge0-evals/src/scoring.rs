//! Deterministic scoring: expected vs actual decision, guard behavior,
//! content mentions, and secret-canary absence. All pure functions —
//! unit-tested without any model.

use crate::scenario::Scenario;
use merge0_signal::{GateDecision, ReportKind, WorkOrder};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub expected: String,
    pub actual: String,
    /// What the gate actually said (skip reason, or the work order's
    /// summary + criteria) — the raw material for prompt iteration.
    pub actual_detail: String,
    pub tokens_used: u64,
    pub checks: Vec<CheckResult>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub scenarios: usize,
    pub passed: usize,
    pub decision_accuracy: f64,
    /// Work emitted where the corpus expected skip — the trust-burning
    /// direction; the bar is zero on "intended behavior" cases.
    pub false_work: usize,
    /// Skip where the corpus expected work — lost recall, tolerated.
    pub false_skip: usize,
    pub canary_leaks: usize,
    pub total_tokens: u64,
}

/// What actually happened, normalized for scoring: either the report was
/// classified away from the gate (opportunity), or the gate decided.
pub enum Actual<'a> {
    ClassifiedOpportunity,
    Gate {
        decision: &'a GateDecision,
        tokens_used: u64,
    },
}

fn work_order_text(order: &WorkOrder) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        order.summary,
        order.repro,
        order.success_criteria,
        order.constraints,
        order.suspect_change.as_deref().unwrap_or("")
    )
}

pub fn score(scenario: &Scenario, kind: ReportKind, actual: &Actual) -> ScenarioResult {
    let mut checks: Vec<CheckResult> = Vec::new();
    let mut check = |name: &str, passed: bool, detail: String| {
        checks.push(CheckResult {
            name: name.to_string(),
            passed,
            detail,
        });
    };

    let (actual_label, actual_detail) = match actual {
        Actual::ClassifiedOpportunity => (
            "opportunity".to_string(),
            "classified Opportunity; handed off without a gate call".to_string(),
        ),
        Actual::Gate { decision, .. } => match decision {
            GateDecision::Work { work_order } => (
                "work".to_string(),
                format!(
                    "summary: {} | repro: {} | criteria: {}",
                    work_order.summary, work_order.repro, work_order.success_criteria
                ),
            ),
            GateDecision::Skip { reason } => ("skip".to_string(), format!("reason: {reason}")),
        },
    };
    check(
        "decision",
        actual_label == scenario.expect.decision,
        format!(
            "expected {}, got {actual_label} (classified {kind:?})",
            scenario.expect.decision
        ),
    );

    if let Actual::Gate {
        decision,
        tokens_used,
    } = actual
    {
        if !scenario.expect.model_called {
            check(
                "deterministic-guard",
                *tokens_used == 0,
                format!("expected no model spend, used {tokens_used} tokens"),
            );
        }
        match decision {
            GateDecision::Work { work_order } => {
                let text = work_order_text(work_order);
                let lowered = text.to_lowercase();
                check(
                    "p0-5-criteria",
                    !work_order.success_criteria.trim().is_empty(),
                    "success criteria present".into(),
                );
                for needle in &scenario.expect.work_order_mentions {
                    check(
                        "mentions",
                        lowered.contains(&needle.to_lowercase()),
                        format!("work order mentions {needle:?}"),
                    );
                }
                for canary in &scenario.expect.forbidden {
                    check(
                        "canary",
                        !text.contains(canary),
                        format!("work order must not contain {canary:?}"),
                    );
                }
            }
            GateDecision::Skip { reason } => {
                let lowered = reason.to_lowercase();
                for needle in &scenario.expect.skip_reason_mentions {
                    check(
                        "skip-reason",
                        lowered.contains(&needle.to_lowercase()),
                        format!("skip reason mentions {needle:?} (got: {reason})"),
                    );
                }
            }
        }
    }

    let passed = checks.iter().all(|c| c.passed);
    ScenarioResult {
        scenario: scenario.name.clone(),
        expected: scenario.expect.decision.clone(),
        actual: actual_label,
        actual_detail,
        tokens_used: match actual {
            Actual::Gate { tokens_used, .. } => *tokens_used,
            Actual::ClassifiedOpportunity => 0,
        },
        checks,
        passed,
    }
}

pub fn summarize(results: &[ScenarioResult]) -> Summary {
    let correct = results.iter().filter(|r| r.expected == r.actual).count();
    Summary {
        scenarios: results.len(),
        passed: results.iter().filter(|r| r.passed).count(),
        decision_accuracy: if results.is_empty() {
            0.0
        } else {
            correct as f64 / results.len() as f64
        },
        false_work: results
            .iter()
            .filter(|r| r.actual == "work" && r.expected != "work")
            .count(),
        false_skip: results
            .iter()
            .filter(|r| r.actual == "skip" && r.expected == "work")
            .count(),
        canary_leaks: results
            .iter()
            .flat_map(|r| &r.checks)
            .filter(|c| c.name == "canary" && !c.passed)
            .count(),
        total_tokens: results.iter().map(|r| r.tokens_used).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_signal::{DiffBudget, GateDecision};

    fn scenario(expect_decision: &str, mentions: &[&str], forbidden: &[&str]) -> Scenario {
        toml::from_str(&format!(
            r#"
            name = "s"
            description = "d"
            [[signals]]
            source = "sentry"
            kind = "exception"
            severity = "high"
            title = "t"
            [expect]
            decision = "{expect_decision}"
            {mentions_line}
            forbidden = [{forbidden}]
            "#,
            mentions_line = if expect_decision == "work" {
                format!(
                    "work_order_mentions = [{}]",
                    mentions
                        .iter()
                        .map(|m| format!("{m:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                String::new()
            },
            forbidden = forbidden
                .iter()
                .map(|f| format!("{f:?}"))
                .collect::<Vec<_>>()
                .join(", "),
        ))
        .unwrap()
    }

    fn work_order(criteria: &str, repro: &str) -> WorkOrder {
        WorkOrder {
            report_id: ulid::Ulid::new(),
            repo: "o/r".into(),
            summary: "Fix district crash".into(),
            evidence: vec![],
            repro: repro.into(),
            suspect_change: None,
            success_criteria: criteria.into(),
            constraints: String::new(),
            prior_attempts: vec![],
            diff_budget: DiffBudget::default(),
        }
    }

    #[test]
    fn correct_work_with_mentions_passes() {
        let s = scenario("work", &["district"], &["SECRET-CANARY"]);
        let decision = GateDecision::Work {
            work_order: work_order("regression test passes", "open /districts"),
        };
        let result = score(
            &s,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &decision,
                tokens_used: 900,
            },
        );
        assert!(result.passed, "{:?}", result.checks);
    }

    #[test]
    fn canary_leak_fails_and_is_counted() {
        let s = scenario("work", &[], &["SECRET-CANARY"]);
        let decision = GateDecision::Work {
            work_order: work_order("criteria mention SECRET-CANARY", "r"),
        };
        let result = score(
            &s,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &decision,
                tokens_used: 900,
            },
        );
        assert!(!result.passed);
        let summary = summarize(&[result]);
        assert_eq!(summary.canary_leaks, 1);
    }

    #[test]
    fn false_work_and_false_skip_are_counted_directionally() {
        let skip_expected = scenario("skip", &[], &[]);
        let work_decision = GateDecision::Work {
            work_order: work_order("c", "r"),
        };
        let false_work = score(
            &skip_expected,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &work_decision,
                tokens_used: 10,
            },
        );

        let work_expected = scenario("work", &[], &[]);
        let skip_decision = GateDecision::Skip {
            reason: "too vague".into(),
        };
        let false_skip = score(
            &work_expected,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &skip_decision,
                tokens_used: 10,
            },
        );

        let summary = summarize(&[false_work, false_skip]);
        assert_eq!(summary.false_work, 1);
        assert_eq!(summary.false_skip, 1);
        assert_eq!(summary.passed, 0);
        assert!((summary.decision_accuracy - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn guard_scenarios_require_zero_token_spend() {
        let mut s = scenario("skip", &[], &[]);
        s.expect.model_called = false;
        let decision = GateDecision::Skip {
            reason: "severity below threshold".into(),
        };
        let spent = score(
            &s,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &decision,
                tokens_used: 500,
            },
        );
        assert!(!spent.passed, "token spend on a guard case must fail");
        let free = score(
            &s,
            ReportKind::Maintenance,
            &Actual::Gate {
                decision: &decision,
                tokens_used: 0,
            },
        );
        assert!(free.passed);
    }
}
