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
    /// How many times the scenario was run (1 unless it declares
    /// `samples`), and what fraction of those runs passed. A single-sample
    /// scenario reports 1.0 or 0.0 — same information as `passed`, stated
    /// so that every row in the results JSON is comparable.
    pub samples: usize,
    pub pass_rate: f64,
}

/// Fold repeated runs of one scenario into the single row the summary and
/// the corpus bar operate on.
///
/// Three deliberate choices:
/// - **passed** is `pass_rate >= min_pass_rate`, so a scenario the corpus
///   documents as a judgment call is allowed to wobble, and one that does
///   not is still all-or-nothing.
/// - **checks from every sample are kept**, because a canary that leaks in
///   one run out of five has leaked. The bar counts leaks across all of
///   them; it must never be satisfied by a lucky representative sample.
/// - **the reported decision is the modal one**, and the detail comes from
///   a failing run when there is one — that is the run worth reading.
pub fn fold_samples(mut samples: Vec<ScenarioResult>, min_pass_rate: f64) -> ScenarioResult {
    assert!(!samples.is_empty(), "a scenario runs at least once");
    let total = samples.len();
    let passed_count = samples.iter().filter(|r| r.passed).count();
    let pass_rate = passed_count as f64 / total as f64;
    if total == 1 {
        let mut only = samples.pop().expect("one sample");
        only.samples = 1;
        only.pass_rate = pass_rate;
        return only;
    }

    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for sample in &samples {
        *counts.entry(sample.actual.as_str()).or_default() += 1;
    }
    let modal = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(label, _)| label.to_string())
        .expect("at least one decision");

    let detail_source = samples
        .iter()
        .find(|r| !r.passed)
        .unwrap_or(&samples[0])
        .actual_detail
        .clone();
    let tokens_used = samples.iter().map(|r| r.tokens_used).sum();
    let checks = samples.into_iter().flat_map(|r| r.checks).collect();

    ScenarioResult {
        scenario: String::new(), // filled by the caller from the scenario
        expected: String::new(),
        actual: modal,
        actual_detail: detail_source,
        tokens_used,
        checks,
        passed: pass_rate >= min_pass_rate,
        samples: total,
        pass_rate,
    }
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
        samples: 1,
        pass_rate: if passed { 1.0 } else { 0.0 },
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
            report_id: ulid::Ulid::generate(),
            repo: "o/r".into(),
            summary: "Fix district crash".into(),
            evidence: vec![],
            repro: repro.into(),
            suspect_change: None,
            success_criteria: criteria.into(),
            constraints: String::new(),
            prior_attempts: vec![],
            diff_budget: DiffBudget::default(),
            confidence: Default::default(),
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

#[cfg(test)]
mod fold_tests {
    use super::*;

    fn sample(actual: &str, passed: bool, checks: Vec<CheckResult>) -> ScenarioResult {
        ScenarioResult {
            scenario: "s".into(),
            expected: "work".into(),
            actual: actual.into(),
            actual_detail: format!("detail for {actual}"),
            tokens_used: 100,
            checks,
            passed,
            samples: 1,
            pass_rate: if passed { 1.0 } else { 0.0 },
        }
    }

    fn canary_leak() -> CheckResult {
        CheckResult {
            name: "canary".into(),
            passed: false,
            detail: "leaked".into(),
        }
    }

    #[test]
    fn a_single_sample_is_unchanged_apart_from_its_rate() {
        let folded = fold_samples(vec![sample("work", true, vec![])], 1.0);
        assert!(folded.passed);
        assert_eq!(folded.samples, 1);
        assert_eq!(folded.pass_rate, 1.0);
        assert_eq!(folded.actual, "work");
    }

    #[test]
    fn a_documented_judgment_call_may_wobble_within_its_declared_rate() {
        let samples = vec![
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("skip", false, vec![]),
        ];
        let folded = fold_samples(samples, 0.8);
        assert!(folded.passed, "4/5 meets a declared 0.8 bar");
        assert_eq!(folded.pass_rate, 0.8);
        assert_eq!(folded.samples, 5);
    }

    #[test]
    fn falling_below_the_declared_rate_fails() {
        let samples = vec![
            sample("work", true, vec![]),
            sample("skip", false, vec![]),
            sample("skip", false, vec![]),
        ];
        let folded = fold_samples(samples, 0.8);
        assert!(!folded.passed);
        // The modal decision is what the gate mostly does, not what we hoped.
        assert_eq!(folded.actual, "skip");
    }

    /// The safety-critical property. A canary that leaks once in five runs
    /// has leaked; folding must not let a lucky representative sample hide
    /// it, even when the scenario's pass-rate bar is satisfied.
    #[test]
    fn a_leak_in_any_sample_survives_folding() {
        let samples = vec![
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("work", true, vec![]),
            sample("work", false, vec![canary_leak()]),
        ];
        let folded = fold_samples(samples, 0.8);
        assert!(folded.passed, "the rate bar is met");
        let summary = summarize(&[folded]);
        assert_eq!(
            summary.canary_leaks, 1,
            "a leak in one run of five is still a leak"
        );
    }

    /// A failing run is the one worth reading, so it supplies the detail
    /// even when the scenario passes overall.
    #[test]
    fn the_reported_detail_comes_from_a_failing_run_when_there_is_one() {
        let samples = vec![
            sample("work", true, vec![]),
            sample("skip", false, vec![]),
            sample("work", true, vec![]),
        ];
        let folded = fold_samples(samples, 0.5);
        assert!(folded.passed);
        assert_eq!(folded.actual_detail, "detail for skip");
    }
}
