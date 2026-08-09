//! Repo hygiene: past bugs promoted from prose into enforcement.
//!
//! CLAUDE.md's self-improvement protocol says to prefer promoting recurring
//! lessons into *enforced artifacts* over letting a lesson list grow — an
//! ever-growing list is context rot, and prose has never once failed a
//! build. This file is that promotion, and it mirrors the mechanism
//! hierarchy Merge0 applies to customer repos (`merge0-hardening`:
//! LintRule > RegressionTest > IntentAmendment) back onto Merge0 itself.
//!
//! Every rule here is a bug that actually happened. A rule earns its place
//! by being (a) a real incident and (b) precisely detectable — a lint with
//! false positives trains people to ignore it, which is worse than no lint.
//! If a rule ever becomes wrong, delete it deliberately; do not special-case
//! around it.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

/// Every `.rs` file under `crates/` and `ee/`, excluding this file (which
/// necessarily contains the very patterns it forbids, as string literals).
fn rust_sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = Vec::new();
    for top in ["crates", "ee"] {
        walk(&root.join(top), &mut out);
    }
    out.retain(|p| !p.ends_with("tests/repo_hygiene.rs"));
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds vendored sources — never our code.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn rel(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// **Incident (twice).** Postgres `SUM()` over BIGINT returns NUMERIC, so
/// `query_scalar::<i64>` fails at runtime with `ColumnDecode` — invisible to
/// every compile-time check. It bit the telemetry query, then bit the
/// budget-ledger and escalation queries months later.
///
/// The rule is absolute rather than clever: any line summing must carry the
/// cast. Assigning to a BIGINT column coerces fine without it, but keeping
/// one uniform shape is what makes the rule enforceable with no exceptions
/// for a future reader to misjudge.
#[test]
fn sum_in_sql_is_always_cast_to_bigint() {
    let mut offenders = Vec::new();
    for file in rust_sources() {
        let text = std::fs::read_to_string(&file).expect("source readable");
        for (i, line) in text.lines().enumerate() {
            if line.contains("SUM(") && !line.contains("::BIGINT") {
                offenders.push(format!("{}:{}: {}", rel(&file), i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "Postgres SUM() returns NUMERIC — decoding it as i64 panics at \
         runtime. Add `::BIGINT` to each:\n{}",
        offenders.join("\n")
    );
}

/// **Incident.** `auth_header()` returns the raw `Authorization` value
/// *including* the `Bearer ` scheme prefix. `require_bearer` strips it
/// internally, so hand-rolled comparisons that skip the strip silently
/// reject every valid credential — the broker's credential endpoint 401'd
/// on correct runner keys until an integration test caught it.
///
/// Any use of the raw header must therefore either go through
/// `require_bearer` or visibly strip the prefix.
#[test]
fn raw_auth_header_is_never_compared_with_the_bearer_prefix_attached() {
    let mut offenders = Vec::new();
    for file in rust_sources() {
        let text = std::fs::read_to_string(&file).expect("source readable");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // `expose_for_auth_header(` is an unrelated Secret accessor, and
            // the declaration of `auth_header` itself is not a use of it.
            let is_call = line.contains("auth_header(")
                && !line.contains("_auth_header(")
                && !line.contains("fn auth_header(");
            if !is_call || line.trim_start().starts_with("//") {
                continue;
            }
            // The strip may land a couple of lines below in a method chain.
            let window = lines[i..(i + 4).min(lines.len())].join("\n");
            let handled =
                window.contains("require_bearer") || window.contains("strip_prefix(\"Bearer ");
            if !handled {
                offenders.push(format!("{}:{}: {}", rel(&file), i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "auth_header() keeps the \"Bearer \" prefix — compare via \
         require_bearer() or strip_prefix(\"Bearer \") first:\n{}",
        offenders.join("\n")
    );
}

/// **Incident.** An adapter whose signals all land below the shipped gate's
/// `min_severity` is a *silently dead source*: it ingests happily, and every
/// signal is guard-skipped before the gate ever sees it. Slack and Asana
/// shipped that way until the arithmetic was checked by hand.
///
/// A source is only alive if it can, at least sometimes, produce a signal
/// that clears the floor — so each adapter's goldens must contain at least
/// one. This deliberately reads the *shipped* `config/gate.toml`, so raising
/// the floor fails here instead of quietly killing a source.
#[test]
fn every_adapter_can_produce_a_signal_that_clears_the_gate_floor() {
    let root = repo_root();
    let gate = std::fs::read_to_string(root.join("config/gate.toml")).expect("gate config");
    let floor = gate
        .lines()
        .find_map(|l| l.trim().strip_prefix("min_severity = "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("gate.toml declares min_severity");
    let floor_rank = rank(&floor);

    let mut dead = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates dir")
        .flatten()
    {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("merge0-adapter-") {
            continue;
        }
        let fixtures = dir.join("tests/fixtures");
        let mut best: Option<(String, u8)> = None;
        let Ok(files) = std::fs::read_dir(&fixtures) else {
            continue; // adapter without goldens is caught by its own tests
        };
        for f in files.flatten() {
            let path = f.path();
            if !path.to_string_lossy().ends_with(".expected.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("fixture readable");
            let value: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
            for signal in signals_of(&value) {
                if let Some(sev) = signal.get("severity").and_then(|s| s.as_str()) {
                    let r = rank(sev);
                    if best.as_ref().is_none_or(|(_, b)| r > *b) {
                        best = Some((sev.to_string(), r));
                    }
                }
            }
        }
        match best {
            Some((_, r)) if r >= floor_rank => {}
            Some((sev, _)) => dead.push(format!("{name}: highest golden severity is {sev:?}")),
            None => {} // no expected signals at all: that adapter's own tests own it
        }
    }
    assert!(
        dead.is_empty(),
        "these sources can never reach the gate (shipped floor is {floor:?}) — \
         every signal they emit is guard-skipped before triage:\n{}",
        dead.join("\n")
    );
}

/// Expected-golden files are either a bare array of Signals or a single one.
fn signals_of(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

fn rank(severity: &str) -> u8 {
    match severity {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        "critical" => 3,
        other => panic!("unknown severity {other:?}"),
    }
}

/// **Incident class (designed-out, kept out).** Merge0 both writes stories
/// to trackers and ingests from them, so an adapter that fails to skip
/// Merge0's own output re-ingests it and triages the system forever.
///
/// The guard is `merge0_signal::ORIGIN_LABEL`, referenced by the writer
/// (delivery) and every reader (adapters). An adapter that inlines the
/// string instead keeps working *today* and silently stops guarding the
/// day the constant changes — the loop returns with no failing test. So
/// adapter sources must name the constant, never the literal.
///
/// Three things legitimately contain the literal and are excluded: vendor
/// JSON fixtures (it is what the tracker actually stores), comments
/// (documenting *why* the label is skipped is the behavior we want), and
/// `#[cfg(test)]` code — you cannot test "skip issues carrying this label"
/// without writing the label down. Only production code is constrained,
/// which is where the silent rot would actually happen. Flagging the other
/// three would be exactly the false-positive noise that teaches people to
/// ignore this file.
#[test]
fn adapters_reference_the_origin_label_constant_not_the_literal() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for file in rust_sources() {
        let path = rel(&file);
        let is_adapter_src = path.starts_with("crates/merge0-adapter-") && path.contains("/src/");
        if !is_adapter_src {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("source readable");
        // Everything from the first `#[cfg(test)]` on is test code.
        let production = match text.find("#[cfg(test)]") {
            Some(at) => &text[..at],
            None => &text[..],
        };
        for (i, line) in production.lines().enumerate() {
            let is_comment = line.trim_start().starts_with("//");
            if !is_comment && line.contains(merge0_signal::ORIGIN_LABEL) {
                offenders.push(format!("{path}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    let _ = root;
    assert!(
        offenders.is_empty(),
        "adapters must skip Merge0's own artifacts via merge0_signal::ORIGIN_LABEL, \
         not a hardcoded copy that silently rots when the constant changes:\n{}",
        offenders.join("\n")
    );
}

/// Returns the part of a source file before the first `#[cfg(test)]` —
/// production code, which is where silent rot actually happens. Tests
/// legitimately write down the patterns their subject forbids.
fn production_of(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(at) => &text[..at],
        None => text,
    }
}

/// **Incident.** The gate's prompt builder cut the customer's intent doc to
/// `max_section_chars` with a head truncation. The shipped `MERGE0.md`
/// template ends with *Constraints for generated fixes* and then the machine
/// fence, so on any real-sized doc the first content dropped was the
/// constraints and the amendments Merge0 earned from past incidents —
/// the exact content that prevents a bad Work Order. Nothing failed; the
/// gate just quietly decided with less.
///
/// Intent must therefore be *selected* (`merge0_context::intent::
/// relevant_intent`), which keeps sections whole and discloses what it
/// dropped, never head-truncated. Evidence text may still be truncated —
/// that is a size budget on quoted vendor noise, not on the rules.
#[test]
fn intent_is_never_blind_truncated() {
    let mut offenders = Vec::new();
    for file in rust_sources() {
        let text = std::fs::read_to_string(&file).expect("source readable");
        for (i, line) in production_of(&text).lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            let truncating =
                trimmed.contains("truncate_with_marker(") || trimmed.contains("truncate_to(");
            if truncating && trimmed.to_ascii_lowercase().contains("intent") {
                offenders.push(format!("{}:{}: {}", rel(&file), i + 1, trimmed));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "intent must be selected with merge0_context::intent::relevant_intent \
         (whole sections + disclosure of omissions), never head-truncated — \
         truncation silently drops the constraints that sit at the end of a \
         real MERGE0.md:\n{}",
        offenders.join("\n")
    );
}

/// **Incident.** `IntentDoc::human_text()` returns the doc *without* the
/// machine-managed fence. The server used it to build the text handed to
/// the gate, so every `IntentAmendment` the hardening pass had ever written
/// — mechanism 3 of the LintRule > RegressionTest > IntentAmendment
/// hierarchy — was written to a location nothing read. A whole prevention
/// mechanism was inert, and no test noticed because the writes succeeded.
///
/// Fence-aware handling belongs in `merge0-context`, which labels the
/// managed section and passes it on. Everywhere else, carry the doc whole.
#[test]
fn the_machine_fence_is_stripped_only_inside_merge0_context() {
    let mut offenders = Vec::new();
    for file in rust_sources() {
        let path = rel(&file);
        if path.starts_with("crates/merge0-context/") {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("source readable");
        for (i, line) in production_of(&text).lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains("human_text()") {
                offenders.push(format!("{path}:{}: {}", i + 1, trimmed));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "human_text() drops the machine-managed fence, which is where \
         merge0-hardening writes constraints earned from real incidents. \
         Pass the intent doc whole and let merge0-context decide what the \
         gate sees:\n{}",
        offenders.join("\n")
    );
}

/// **Incident (live-poller e2e, 2026-08-09).** A local end-to-end run with
/// real pollers against fake vendors landed three signals — and triage
/// selected one. Mixpanel and OpenPanel were absent from every shipped
/// scout's `sources` list, and the audit that followed found datadog,
/// loopforge, otel, and webhook had shipped the same way: ingesting
/// happily, selectable by nothing, dead since the day they were added.
///
/// The existing gate-floor rule catches an adapter whose signals are too
/// weak to matter; this one catches the layer above — an adapter whose
/// signals never reach triage at all. It runs each adapter's expected
/// goldens (re-stamped to now, so window filters pass) through the REAL
/// scout selection over the SHIPPED `config/scouts/`, no parallel parser
/// to rot: if no shipped scout can select any golden signal from a source,
/// that source is dead and this fails naming it.
#[test]
fn every_adapter_source_is_selectable_by_at_least_one_shipped_scout() {
    let root = repo_root();
    let scouts = merge0_triage::config::load_scouts(&root.join("config/scouts"))
        .expect("shipped scouts load");
    let now = chrono::Utc::now();

    // source string -> (adapter dir, any golden signal selected?)
    let mut sources: std::collections::BTreeMap<String, (String, bool)> =
        std::collections::BTreeMap::new();

    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates dir")
        .flatten()
    {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("merge0-adapter-") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.join("tests/fixtures")) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if !path.to_string_lossy().ends_with(".expected.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("fixture readable");
            let value: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
            for raw in signals_of(&value) {
                // The harness normalizes volatile ids to the literal
                // "<ulid>", which no ULID parser accepts — patch it back to
                // a real one. And parse failures are LOUD: a silently
                // skipped golden made the first draft of this rule pass
                // against six dead sources.
                let mut raw = raw.clone();
                if raw.get("id").and_then(|v| v.as_str()) == Some("<ulid>") {
                    raw["id"] = serde_json::json!(ulid::Ulid::new().to_string());
                }
                let mut signal = serde_json::from_value::<merge0_signal::Signal>(raw)
                    .unwrap_or_else(|e| panic!("golden in {} is not a Signal: {e}", rel(&path)));
                // Freshness is the poller's job, not the fixture's: stamp to
                // now so only source/kind/shape decide selectability.
                signal.first_seen = now;
                signal.last_seen = now;
                let selected = !merge0_triage::scouts::union_candidates(
                    &scouts,
                    std::slice::from_ref(&signal),
                    now,
                )
                .expect("shipped scout queries parse")
                .is_empty();
                let entry = sources
                    .entry(signal.source.as_str().to_string())
                    .or_insert((name.clone(), false));
                entry.1 |= selected;
            }
        }
    }

    let dead: Vec<String> = sources
        .iter()
        .filter(|(_, (_, selected))| !selected)
        .map(|(source, (adapter, _))| format!("{source} (goldens in {adapter})"))
        .collect();
    assert!(
        dead.is_empty(),
        "no shipped scout in config/scouts/ can select ANY signal these \
         sources emit — they ingest into the store and then nothing ever \
         reads them, which is a dead source with extra steps. Add the \
         source to an appropriate scout's `sources` list (and check its \
         query's kind filter):\n{}",
        dead.join("\n")
    );
}

/// **Boundary made law (CLAUDE.md invariant 5).** Nothing under `crates/`
/// may depend on `ee/` — that line is what makes the MIT half of the repo
/// genuinely MIT. It has always held by review; as of the open-sourcing
/// pass it is enforced, because the cost of it silently breaking changes
/// the day the repo is public: a leaked dependency wouldn't just be a
/// layering bug, it would relicense someone's build.
///
/// Detection is at the manifest level, where the dependency would have to
/// be declared: any `crates/*/Cargo.toml` naming an ee crate or an `ee/`
/// path. Comments are skipped; `ee/` appearing in prose stays legal.
#[test]
fn no_mit_crate_depends_on_the_ee_directory() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).expect("crates dir").flatten() {
        let manifest = entry.path().join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            let names_ee_crate =
                trimmed.contains("merge0-ee") || trimmed.contains("merge0-hosted");
            let ee_path_dep = trimmed.contains("path") && trimmed.contains("ee/");
            if names_ee_crate || ee_path_dep {
                offenders.push(format!("{}:{}: {}", rel(&manifest), i + 1, trimmed));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "crates/ is MIT and ee/ is not — a dependency across that boundary \
         relicenses the build. Move the code, not the dependency:\n{}",
        offenders.join("\n")
    );
}
