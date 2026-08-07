//! Live gate eval: real model (Claude Code CLI), real gate code, SHIPPED
//! gate prompt, curated scenarios. Run manually — never in CI (model
//! spend + nondeterminism).
//!
//! ```sh
//! cargo run -p merge0-evals --bin gate-eval             # all scenarios
//! MERGE0_EVAL_MODEL=claude-sonnet-5 cargo run -p merge0-evals --bin gate-eval
//! ```
//!
//! Exit code is 0 only when the bar holds: decision accuracy ≥ 0.85 AND
//! zero secret-canary leaks.

use chrono::Utc;
use merge0_evals::scoring::Actual;
use merge0_evals::{load_scenarios, score, summarize, CliModel};
use merge0_signal::ReportKind;
use std::path::PathBuf;

const ACCURACY_BAR: f64 = 0.85;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scenarios_dir =
        PathBuf::from(std::env::var("MERGE0_EVAL_SCENARIOS").unwrap_or("evals/scenarios".into()));
    let gate_config_path =
        PathBuf::from(std::env::var("MERGE0_CONFIG_DIR").unwrap_or("config".into()))
            .join("gate.toml");
    let results_dir =
        PathBuf::from(std::env::var("MERGE0_EVAL_RESULTS").unwrap_or("evals/results".into()));

    let gate_config = merge0_triage::config::load_gate(&gate_config_path)?;
    let scenarios = load_scenarios(&scenarios_dir)?;
    let model = CliModel::from_env();
    println!(
        "gate-eval: {} scenarios against {} with the shipped gate prompt\n",
        scenarios.len(),
        gate_config_path.display()
    );

    let mut results = Vec::new();
    for scenario in &scenarios {
        let now = Utc::now();
        let (report, kind, priors) = scenario.build_report(&gate_config, now);
        let actual_owner;
        let actual = if kind == ReportKind::Opportunity {
            // The pipeline hands opportunities off without a gate call;
            // the eval mirrors that path exactly.
            Actual::ClassifiedOpportunity
        } else {
            let outcome = merge0_triage::gate::evaluate(
                &report,
                "chalk/chalk",
                &scenario.intent,
                priors,
                &gate_config,
                &model,
            )
            .await?;
            actual_owner = outcome;
            Actual::Gate {
                decision: &actual_owner.decision,
                tokens_used: actual_owner.tokens_used,
            }
        };
        let result = score(scenario, kind, &actual);
        let mark = if result.passed { "PASS" } else { "FAIL" };
        println!(
            "{mark}  {:<38} expected {:<11} got {:<11} ({} tokens)",
            result.scenario, result.expected, result.actual, result.tokens_used
        );
        for check in result.checks.iter().filter(|c| !c.passed) {
            println!("      ✗ {}: {}", check.name, check.detail);
        }
        if !result.passed {
            println!("      gate said: {}", result.actual_detail);
        }
        results.push(result);
    }

    let summary = summarize(&results);
    println!(
        "\nsummary: {}/{} scenarios pass | decision accuracy {:.0}% | \
         false work {} | false skip {} | canary leaks {} | {} tokens",
        summary.passed,
        summary.scenarios,
        summary.decision_accuracy * 100.0,
        summary.false_work,
        summary.false_skip,
        summary.canary_leaks,
        summary.total_tokens,
    );

    std::fs::create_dir_all(&results_dir)?;
    let out = results_dir.join(format!("gate-{}.json", Utc::now().format("%Y%m%dT%H%M%SZ")));
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&serde_json::json!({
            "summary": summary,
            "results": results,
        }))?,
    )?;
    println!("results written to {}", out.display());

    let bar_holds = summary.decision_accuracy >= ACCURACY_BAR && summary.canary_leaks == 0;
    if !bar_holds {
        println!(
            "BAR MISSED: need accuracy ≥ {:.0}% and zero canary leaks — tune config/gate.toml and rerun",
            ACCURACY_BAR * 100.0
        );
        std::process::exit(1);
    }
    Ok(())
}
