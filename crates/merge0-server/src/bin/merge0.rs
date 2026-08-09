//! `merge0 triage` — the no-infrastructure quickstart: one vendor export
//! in, evidence-backed gate decisions out. No server, no Postgres, no
//! GitHub App — the same adapters, clustering, and gate the full loop
//! runs, driven once over a file, with the Claude Code CLI (or any
//! compatible CLI) as the model so your existing login is the only
//! credential involved.
//!
//! ```sh
//! # A Sentry issues export (the API's JSON array works as-is):
//! merge0 triage --source sentry --file issues.json
//!
//! # Any source, using the envelope shape from that adapter's fixtures:
//! merge0 triage --source posthog --file envelope.json
//! ```

use merge0_model::{CliModel, Model};
use merge0_server::handlers::ingest::adapter_for;
use merge0_signal::{GateDecision, Signal};
use merge0_triage::cluster::{assemble_report, classify, cluster_signals};
use merge0_triage::config::GateConfig;

const USAGE: &str = "\
merge0 triage — one-shot local triage over a vendor export (no server, no DB)

USAGE:
  merge0 triage --source <source> [--file <path>|-] [options]

OPTIONS:
  --source <name>   adapter to normalize with (sentry, posthog, datadog,
                    jira, linear, zendesk, ... — any ingest source)
  --file <path>     vendor export JSON; '-' or omitted reads stdin.
                    A raw Sentry issues array is accepted as-is; other
                    sources take the {endpoint, context, payload} envelope
                    (see that adapter's tests/fixtures/ for the shape)
  --gate <path>     gate config (default: config/gate.toml)
  --intent <path>   product-intent doc the gate reads (default: built-in
                    template — a real MERGE0.md gives better decisions)
  --model <id>      model override passed to the CLI backend
  --cli <binary>    model CLI binary (default: claude)
  --json            machine-readable JSON on stdout instead of text
";

#[tokio::main]
async fn main() {
    if let Err(message) = run(std::env::args().skip(1).collect()).await {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

#[derive(Debug)]
struct Args {
    source: String,
    file: Option<String>,
    gate: String,
    intent: Option<String>,
    model: Option<String>,
    cli: String,
    json: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    if argv.first().map(String::as_str) != Some("triage") {
        return Err(format!("expected the `triage` subcommand\n\n{USAGE}"));
    }
    let mut args = Args {
        source: String::new(),
        file: None,
        gate: "config/gate.toml".into(),
        intent: None,
        model: None,
        cli: "claude".into(),
        json: false,
    };
    let mut it = argv[1..].iter();
    while let Some(flag) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value\n\n{USAGE}"))
        };
        match flag.as_str() {
            "--source" => args.source = value("--source")?,
            "--file" => args.file = Some(value("--file")?),
            "--gate" => args.gate = value("--gate")?,
            "--intent" => args.intent = Some(value("--intent")?),
            "--model" => args.model = Some(value("--model")?),
            "--cli" => args.cli = value("--cli")?,
            "--json" => args.json = true,
            "--help" | "-h" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown flag {other:?}\n\n{USAGE}")),
        }
    }
    if args.source.is_empty() {
        return Err(format!("--source is required\n\n{USAGE}"));
    }
    Ok(args)
}

/// Wrap bare vendor responses in the envelope the adapters expect. A JSON
/// object already carrying `endpoint` passes through; a raw Sentry issues
/// array becomes the `issues` envelope (that's THE quickstart artifact —
/// `GET /api/0/projects/{org}/{project}/issues/` verbatim).
fn envelope_for(source: &str, input: serde_json::Value) -> Result<serde_json::Value, String> {
    if input.get("endpoint").is_some() {
        return Ok(input);
    }
    match (source, &input) {
        ("sentry", serde_json::Value::Array(_)) => Ok(serde_json::json!({
            "endpoint": "issues",
            "payload": input,
        })),
        _ => Err(format!(
            "input is not an adapter envelope (no \"endpoint\" field). Wrap it as \
             {{\"endpoint\": ..., \"context\": {{...}}, \"payload\": <vendor response>}} — \
             crates/merge0-adapter-{source}/tests/fixtures/ shows working examples"
        )),
    }
}

fn normalize(source: &str, envelope: &serde_json::Value) -> Result<Vec<Signal>, String> {
    let adapter = adapter_for(source)
        .ok_or_else(|| format!("unknown source {source:?} — see README for the source list"))?;
    adapter
        .normalize(envelope)
        .map_err(|e| format!("{source} adapter rejected the input: {e}"))
}

async fn run(argv: Vec<String>) -> Result<(), String> {
    let args = parse_args(&argv)?;

    let raw = match args.file.as_deref() {
        None | Some("-") => {
            use std::io::Read;
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|e| format!("reading stdin: {e}"))?;
            buffer
        }
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?,
    };
    let input: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("input is not JSON: {e}"))?;
    let envelope = envelope_for(&args.source, input)?;
    let signals = normalize(&args.source, &envelope)?;
    if signals.is_empty() {
        return Err("the adapter produced no signals from this input".into());
    }

    let gate_config: GateConfig =
        merge0_triage::config::load_gate(std::path::Path::new(&args.gate)).map_err(|e| {
            format!(
                "gate config unreadable at {} ({e}) — run from the Merge0 repo or pass --gate",
                args.gate
            )
        })?;
    let intent = match &args.intent {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("reading intent {path}: {e}"))?
        }
        None => merge0_context::intent::MERGE0_TEMPLATE.to_string(),
    };

    let model = CliModel::with_binary(&args.cli).model(args.model.clone());
    eprintln!(
        "merge0 triage: {} signal(s) from {}, gating with `{}`…",
        signals.len(),
        args.source,
        args.cli
    );

    let now = chrono::Utc::now();
    let mut results = Vec::new();
    for cluster in cluster_signals(signals) {
        let kind = classify(&cluster.signals, false);
        let report = assemble_report(&cluster, kind, None, &gate_config, now);
        let outcome = merge0_triage::gate::evaluate(
            &report,
            "local/quickstart",
            &intent,
            Vec::new(),
            &gate_config,
            &model as &dyn Model,
        )
        .await
        .map_err(|e| format!("gate call failed: {e}"))?;
        results.push((report, outcome));
    }
    // Highest severity first — same ordering instinct as the inbox.
    results.sort_by(|a, b| b.0.severity.cmp(&a.0.severity));

    if args.json {
        let out: Vec<serde_json::Value> = results
            .iter()
            .map(|(report, outcome)| {
                serde_json::json!({
                    "report": report,
                    "decision": outcome.decision,
                    "tokens_used": outcome.tokens_used,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out).expect("serializes")
        );
        return Ok(());
    }

    for (report, outcome) in &results {
        println!("\n━━ [{:?}] {}", report.severity, report.title);
        println!("   {}", report.summary);
        match &outcome.decision {
            GateDecision::Work { work_order } => {
                println!(
                    "   GATE → WORK ORDER ({:?} confidence)",
                    work_order.confidence
                );
                println!("     summary:  {}", work_order.summary);
                println!("     repro:    {}", work_order.repro);
                println!("     success:  {}", work_order.success_criteria);
            }
            GateDecision::Skip { reason } => {
                println!("   GATE → SKIP: {reason}");
            }
        }
    }
    println!(
        "\n{} report(s). The full loop turns approved work orders into \
         test-passing PRs — README “Quick start”.",
        results.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentry_issue() -> serde_json::Value {
        serde_json::json!([{
            "id": "9001",
            "shortId": "ACME-9",
            "title": "TypeError: Cannot read properties of undefined",
            "permalink": "https://sentry.example.com/organizations/acme/issues/9001/",
            "level": "error",
            "metadata": {"type": "TypeError", "value": "undefined"},
            "userCount": 33,
            "firstSeen": "2026-08-06T04:00:00Z",
            "lastSeen": "2026-08-08T04:00:00Z"
        }])
    }

    #[test]
    fn bare_sentry_arrays_are_wrapped_and_normalize() {
        let envelope = envelope_for("sentry", sentry_issue()).unwrap();
        assert_eq!(envelope["endpoint"], "issues");
        let signals = normalize("sentry", &envelope).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, merge0_signal::Source::Sentry);
    }

    #[test]
    fn non_enveloped_input_for_other_sources_errors_with_the_fixture_pointer() {
        let err = envelope_for("posthog", serde_json::json!({"results": []})).unwrap_err();
        assert!(err.contains("merge0-adapter-posthog/tests/fixtures"));
        // …and an enveloped input passes straight through.
        let envelope = serde_json::json!({"endpoint": "x", "payload": {}});
        assert_eq!(envelope_for("posthog", envelope.clone()).unwrap(), envelope);
    }

    #[test]
    fn args_require_the_subcommand_and_a_source() {
        assert!(parse_args(&["triage".into()])
            .unwrap_err()
            .contains("--source"));
        assert!(parse_args(&["serve".into()])
            .unwrap_err()
            .contains("triage"));
        let args = parse_args(&[
            "triage".into(),
            "--source".into(),
            "sentry".into(),
            "--json".into(),
        ])
        .unwrap();
        assert_eq!(args.source, "sentry");
        assert!(args.json);
    }
}
