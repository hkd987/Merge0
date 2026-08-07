//! The meta-scout (PRD §5d): deterministic, evidence-linked config-change
//! proposals against `config/gate.toml`.
//!
//! Two rules, both documented and both strictly artifact-level (the §5d
//! invariant — never the prompt, never the executor):
//!
//! - **(a) Precision gate:** `gate_precision < 0.7` with at least
//!   [`MIN_VERDICTS`] verdicts in the window → propose raising
//!   `min_severity` one level ([`next_severity`], saturating at
//!   `critical`). The edit is textual surgery on the one assignment line —
//!   every other byte of the file, prompt string and comments included, is
//!   preserved verbatim.
//! - **(b) Intended-behavior trend:** `dismissals["intended_behavior"] >= 3`
//!   → the proposal asks the *human* to review the recurring
//!   intended-behavior dismissals; the only file change is an appended
//!   `# meta: ...` comment line in gate.toml. The gate prompt is never
//!   edited mechanically — prompt amendments are a human judgment call.
//!
//! Proposal bodies cite the metric values that motivated them (PRD §5d:
//! every learned change is evidence-linked), and [`open_meta_pr`] lands each
//! proposal as an ordinary `[meta]`-prefixed PR: same gate, same inbox,
//! same human merge, full rollback.

use merge0_github::{GitHubApi, GitHubError, PrInfo, RepoRef};
use merge0_signal::{Severity, TelemetrySnapshot};

use crate::adapter::{
    dismissals_by_reason, intended_dismissals, GATE_PRECISION_TARGET, INTENDED_DISMISSALS_THRESHOLD,
};

/// Rule (a) needs a meaningful sample before proposing config changes.
pub const MIN_VERDICTS: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("github error: {0}")]
    GitHub(#[from] GitHubError),
}

/// An evidence-linked config-change proposal: one file, new content, and a
/// PR title/body citing the metrics that motivated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProposal {
    /// Repo-relative path of the config file, e.g. `config/gate.toml`.
    pub path: String,
    pub new_content: String,
    pub title: String,
    pub body: String,
}

const GATE_TOML_PATH: &str = "config/gate.toml";

pub struct MetaScout;

impl MetaScout {
    /// Evaluate the deterministic proposal rules (see module docs) against a
    /// telemetry snapshot and the current `config/gate.toml` text.
    ///
    /// Never panics on untrusted input: an unparsable gate.toml, a missing
    /// `min_severity` key, or a key already at `critical` simply produces no
    /// rule-(a) proposal.
    pub fn propose_config_changes(
        snapshot: &TelemetrySnapshot,
        current_gate_toml: &str,
    ) -> Vec<ConfigProposal> {
        let mut proposals = Vec::new();
        if let Some(p) = propose_min_severity_raise(snapshot, current_gate_toml) {
            proposals.push(p);
        }
        if let Some(p) = propose_intended_behavior_review(snapshot, current_gate_toml) {
            proposals.push(p);
        }
        proposals
    }
}

/// One step up the severity ladder, saturating at `critical`.
pub fn next_severity(severity: Severity) -> Severity {
    match severity {
        Severity::Low => Severity::Medium,
        Severity::Medium => Severity::High,
        Severity::High => Severity::Critical,
        Severity::Critical => Severity::Critical,
    }
}

/// Rule (a): low gate precision over a sufficient sample → raise
/// `min_severity` one level.
fn propose_min_severity_raise(
    snapshot: &TelemetrySnapshot,
    current_gate_toml: &str,
) -> Option<ConfigProposal> {
    let counts = &snapshot.counts;
    let precision = snapshot.gate_precision?;
    let dismissed: u64 = counts.dismissals.values().sum();
    let verdicts = counts.reports_approved + dismissed;
    if precision >= GATE_PRECISION_TARGET || verdicts < MIN_VERDICTS {
        return None;
    }

    let current = read_min_severity(current_gate_toml)?;
    let raised = next_severity(current);
    if raised == current {
        return None; // already at critical; nothing to raise.
    }
    let new_content = replace_min_severity(current_gate_toml, raised)?;

    let current_str = severity_str(current);
    let raised_str = severity_str(raised);
    Some(ConfigProposal {
        path: GATE_TOML_PATH.to_string(),
        new_content,
        title: format!("Raise gate min_severity from {current_str} to {raised_str}"),
        body: format!(
            "## Meta-scout proposal (PRD \u{a7}5d)\n\n\
             Gate precision {precision_pct:.1}% over the {window}-day window \u{2014} below \
             the {target_pct:.0}% target (PRD leading indicator).\n\n\
             **Evidence:** {approved} approved / {verdicts} verdicts; dismissals by \
             reason: {dismissals}.\n\n\
             **Proposed change:** raise `min_severity` from \"{current_str}\" to \
             \"{raised_str}\" in `{GATE_TOML_PATH}` so lower-severity reports stop \
             reaching reviewers while precision recovers. Only that key changes; \
             prompt and all other settings are untouched.\n\n\
             Rollback is an ordinary `git revert` of this PR.\n",
            precision_pct = precision * 100.0,
            target_pct = GATE_PRECISION_TARGET * 100.0,
            window = counts.window_days,
            approved = counts.reports_approved,
            dismissals = dismissals_by_reason(counts),
        ),
    })
}

/// Rule (b): a rising intended-behavior dismissal trend → ask the human to
/// review; never edit the prompt mechanically. The only mechanical change
/// is an appended gate.toml comment marking the observation.
fn propose_intended_behavior_review(
    snapshot: &TelemetrySnapshot,
    current_gate_toml: &str,
) -> Option<ConfigProposal> {
    let counts = &snapshot.counts;
    let intended = intended_dismissals(counts);
    if intended < INTENDED_DISMISSALS_THRESHOLD {
        return None;
    }

    let comment = format!(
        "# meta: {intended} intended-behavior dismissals in the last window \u{2014} \
         consider intent-doc coverage"
    );
    if current_gate_toml.lines().any(|l| l.trim() == comment) {
        return None; // observation already marked; don't stack duplicates.
    }
    let mut new_content = current_gate_toml.to_string();
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&comment);
    new_content.push('\n');

    Some(ConfigProposal {
        path: GATE_TOML_PATH.to_string(),
        new_content,
        title: "Review recurring intended-behavior dismissals".to_string(),
        body: format!(
            "## Meta-scout observation (PRD \u{a7}5d)\n\n\
             {intended} reports were dismissed as **intended behavior** in the last \
             {window}-day window (threshold: {INTENDED_DISMISSALS_THRESHOLD}). The gate \
             keeps proposing things the product does on purpose \u{2014} that usually \
             means the intent docs (MERGE0.md) don't cover those behaviors yet.\n\n\
             **Requested human review:** look at the recurring intended-behavior \
             dismissals and either extend intent-doc coverage or amend the gate \
             prompt yourself. Merge0 never edits the gate prompt mechanically; the \
             only change in this PR is a `# meta:` comment in `{GATE_TOML_PATH}` \
             recording the observation.\n\n\
             **Evidence:** dismissals by reason: {dismissals}; approved: {approved}.\n",
            window = counts.window_days,
            dismissals = dismissals_by_reason(counts),
            approved = counts.reports_approved,
        ),
    })
}

/// Open a proposal as an ordinary PR: branch `merge0/meta-{slug}`, title
/// prefixed `[meta]` — same gate, same inbox, same human merge.
pub async fn open_meta_pr(
    proposal: &ConfigProposal,
    api: &dyn GitHubApi,
    repo: &RepoRef,
) -> Result<PrInfo, MetaError> {
    let branch = format!("merge0/meta-{}", slugify(&proposal.title));
    let title = format!("[meta] {}", proposal.title);
    let base = api.default_branch(repo).await?;
    api.create_branch_with_files(
        repo,
        &branch,
        &[(proposal.path.clone(), proposal.new_content.clone())],
        &title,
    )
    .await?;
    let pr = api
        .create_pull_request(repo, &branch, &base, &title, &proposal.body)
        .await?;
    Ok(pr)
}

// ---- toml surgery ----

/// Read `min_severity` from gate.toml, tolerating any surrounding content.
/// Returns `None` (never panics) when the file doesn't parse or the key is
/// missing/invalid.
fn read_min_severity(gate_toml: &str) -> Option<Severity> {
    let value: toml::Value = toml::from_str(gate_toml).ok()?;
    let raw = value.get("min_severity")?.as_str()?;
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

/// Replace only the value of the `min_severity` assignment, preserving every
/// other byte of the file (comments, formatting, the prompt string, unknown
/// keys). Returns `None` if no such assignment line exists.
fn replace_min_severity(gate_toml: &str, severity: Severity) -> Option<String> {
    let mut replaced = false;
    let mut out = String::with_capacity(gate_toml.len());
    for line in gate_toml.split_inclusive('\n') {
        if !replaced && is_min_severity_assignment(line) {
            if let Some(new_line) = replace_quoted_value(line, &severity_str(severity)) {
                out.push_str(&new_line);
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
    }
    if !replaced {
        return None;
    }
    // Post-condition guard: the surgical edit must have produced a file that
    // still parses and now carries the raised value — otherwise refuse to
    // propose rather than open a broken-config PR.
    let check: toml::Value = toml::from_str(&out).ok()?;
    (check.get("min_severity")?.as_str()? == severity_str(severity)).then_some(out)
}

fn is_min_severity_assignment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("min_severity")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Swap the interior of the first `"..."` on the line, keeping everything
/// else (indentation, spacing, trailing comments) byte-identical.
fn replace_quoted_value(line: &str, new_value: &str) -> Option<String> {
    let open = line.find('"')?;
    let close = open + 1 + line[open + 1..].find('"')?;
    Some(format!(
        "{}\"{new_value}\"{}",
        &line[..open],
        &line[close + 1..]
    ))
}

/// The wire string of a severity (the serde rename), reusing the schema
/// crate's naming instead of duplicating it.
fn severity_str(severity: Severity) -> String {
    match serde_json::to_value(severity) {
        Ok(serde_json::Value::String(s)) => s,
        // Severity is a unit-variant enum; any other shape is unreachable,
        // but never panic on it either.
        _ => format!("{severity:?}").to_lowercase(),
    }
}

/// Deterministic branch-safe slug from a proposal title.
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let truncated = &slug[..slug.len().min(48)];
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        "proposal".to_string()
    } else {
        truncated.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_signal::telemetry::TelemetryCounts;
    use std::collections::BTreeMap;

    const GATE_TOML: &str = r#"# The gate: Merge0's quality bar (PRD §4).
# Versioned here so gate tuning is an ordinary reviewed PR (meta-loop, §5d).

min_severity = "medium"
max_work_orders_per_run = 3
some_future_key = { nested = true }
prompt = """
You are the gate. min_severity = "decoy" appears here to trip naive edits.
When uncertain, SKIP.
"""
"#;

    fn snapshot(approved: u64, dismissals: &[(&str, u64)]) -> TelemetrySnapshot {
        TelemetrySnapshot::from_counts(TelemetryCounts {
            window_days: 30,
            reports_approved: approved,
            dismissals: dismissals
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<BTreeMap<_, _>>(),
            ..Default::default()
        })
    }

    #[test]
    fn severity_ladder_saturates_at_critical() {
        assert_eq!(next_severity(Severity::Low), Severity::Medium);
        assert_eq!(next_severity(Severity::Medium), Severity::High);
        assert_eq!(next_severity(Severity::High), Severity::Critical);
        assert_eq!(next_severity(Severity::Critical), Severity::Critical);
    }

    #[test]
    fn low_precision_with_enough_verdicts_raises_min_severity() {
        // 2 approved / 5 verdicts = 40% precision.
        let snap = snapshot(2, &[("intended_behavior", 2), ("bad_evidence", 1)]);
        let proposals = MetaScout::propose_config_changes(&snap, GATE_TOML);
        assert_eq!(proposals.len(), 1);
        let p = &proposals[0];
        assert_eq!(p.path, "config/gate.toml");
        assert_eq!(p.title, "Raise gate min_severity from medium to high");

        // The new content parses and only min_severity changed.
        let old: toml::Value = toml::from_str(GATE_TOML).unwrap();
        let new: toml::Value = toml::from_str(&p.new_content).unwrap();
        assert_eq!(new["min_severity"].as_str(), Some("high"));
        for key in ["max_work_orders_per_run", "prompt", "some_future_key"] {
            assert_eq!(old[key], new[key], "{key} must be untouched");
        }

        // Byte-level: every line except the assignment is identical — the
        // prompt string (including its decoy) and comments survive verbatim.
        let old_lines: Vec<_> = GATE_TOML.lines().collect();
        let new_lines: Vec<_> = p.new_content.lines().collect();
        assert_eq!(old_lines.len(), new_lines.len());
        for (old_line, new_line) in old_lines.iter().zip(&new_lines) {
            if old_line.starts_with("min_severity") {
                assert_eq!(*new_line, "min_severity = \"high\"");
            } else {
                assert_eq!(old_line, new_line);
            }
        }

        // Evidence-linked: the body cites the numbers.
        assert!(p.body.contains("40.0%"));
        assert!(p.body.contains("2 approved / 5 verdicts"));
        assert!(p.body.contains("intended_behavior: 2"));
        assert!(p.body.contains("30-day window"));
    }

    #[test]
    fn precision_at_exactly_target_boundary_proposes_nothing() {
        // 7/10 = 0.7 exactly: on-target.
        let snap = snapshot(7, &[("duplicate", 2), ("wont_fix", 1)]);
        assert_eq!(snap.gate_precision, Some(0.7));
        assert!(MetaScout::propose_config_changes(&snap, GATE_TOML).is_empty());
        // Just below the boundary with the same sample size: proposes.
        let snap = snapshot(6, &[("duplicate", 2), ("wont_fix", 2)]);
        assert!(snap.gate_precision.unwrap() < 0.7);
        assert_eq!(MetaScout::propose_config_changes(&snap, GATE_TOML).len(), 1);
    }

    #[test]
    fn too_few_verdicts_proposes_nothing() {
        // 1 approved / 4 verdicts = 25% — but the sample is too small.
        let snap = snapshot(1, &[("duplicate", 2), ("wont_fix", 1)]);
        assert!(MetaScout::propose_config_changes(&snap, GATE_TOML).is_empty());
    }

    #[test]
    fn min_severity_at_critical_saturates_into_no_proposal() {
        let gate = GATE_TOML.replace("min_severity = \"medium\"", "min_severity = \"critical\"");
        let snap = snapshot(1, &[("duplicate", 5)]);
        assert!(MetaScout::propose_config_changes(&snap, &gate).is_empty());
    }

    #[test]
    fn unparsable_or_keyless_toml_never_panics_and_proposes_nothing() {
        let snap = snapshot(1, &[("duplicate", 5)]);
        assert!(MetaScout::propose_config_changes(&snap, "not [valid toml").is_empty());
        assert!(MetaScout::propose_config_changes(&snap, "prompt = \"p\"\n").is_empty());
    }

    #[test]
    fn ladder_walks_low_to_critical_through_the_file() {
        for (from, to) in [("low", "medium"), ("medium", "high"), ("high", "critical")] {
            let gate = GATE_TOML.replace(
                "min_severity = \"medium\"",
                &format!("min_severity = \"{from}\""),
            );
            let snap = snapshot(1, &[("duplicate", 5)]);
            let proposals = MetaScout::propose_config_changes(&snap, &gate);
            assert_eq!(proposals.len(), 1, "from {from}");
            let new: toml::Value = toml::from_str(&proposals[0].new_content).unwrap();
            assert_eq!(new["min_severity"].as_str(), Some(to));
        }
    }

    #[test]
    fn intended_behavior_trend_appends_comment_and_asks_for_review() {
        // Precision healthy (7/10), but 3 intended-behavior dismissals.
        let snap = snapshot(7, &[("intended_behavior", 3)]);
        let proposals = MetaScout::propose_config_changes(&snap, GATE_TOML);
        assert_eq!(proposals.len(), 1);
        let p = &proposals[0];
        assert_eq!(p.title, "Review recurring intended-behavior dismissals");
        // The file change is exactly one appended comment line.
        assert!(p.new_content.starts_with(GATE_TOML));
        let appended = &p.new_content[GATE_TOML.len()..];
        assert_eq!(
            appended,
            "# meta: 3 intended-behavior dismissals in the last window \u{2014} consider intent-doc coverage\n"
        );
        // Still valid toml; prompt untouched.
        let new: toml::Value = toml::from_str(&p.new_content).unwrap();
        let old: toml::Value = toml::from_str(GATE_TOML).unwrap();
        assert_eq!(old["prompt"], new["prompt"]);
        // Body cites the numbers and requests a human decision.
        assert!(p.body.contains('3'));
        assert!(p.body.contains("human review"));
    }

    #[test]
    fn intended_behavior_comment_is_not_stacked_twice() {
        let snap = snapshot(7, &[("intended_behavior", 3)]);
        let first = &MetaScout::propose_config_changes(&snap, GATE_TOML)[0];
        assert!(MetaScout::propose_config_changes(&snap, &first.new_content).is_empty());
    }

    #[test]
    fn both_rules_can_fire_as_separate_proposals() {
        // 2/6 precision AND 3 intended dismissals.
        let snap = snapshot(2, &[("intended_behavior", 3), ("duplicate", 1)]);
        let proposals = MetaScout::propose_config_changes(&snap, GATE_TOML);
        assert_eq!(proposals.len(), 2);
        assert!(proposals[0].title.contains("min_severity"));
        assert!(proposals[1].title.contains("intended-behavior"));
    }

    #[test]
    fn slugify_is_branch_safe() {
        assert_eq!(
            slugify("Raise gate min_severity from medium to high"),
            "raise-gate-min-severity-from-medium-to-high"
        );
        assert_eq!(slugify("!!!"), "proposal");
        assert!(slugify(&"x".repeat(100)).len() <= 48);
    }

    #[tokio::test]
    async fn open_meta_pr_uses_meta_branch_and_title_prefix() {
        use merge0_github::FakeGitHub;

        let snap = snapshot(2, &[("intended_behavior", 2), ("bad_evidence", 1)]);
        let proposal = MetaScout::propose_config_changes(&snap, GATE_TOML)
            .into_iter()
            .next()
            .unwrap();
        let api = FakeGitHub::new();
        let repo = RepoRef::parse("merge0/merge0").unwrap();
        let pr = open_meta_pr(&proposal, &api, &repo).await.unwrap();

        let state = api.state.lock().unwrap();
        assert_eq!(state.created_branches.len(), 1);
        let (_, branch, files, _) = &state.created_branches[0];
        assert_eq!(
            branch,
            "merge0/meta-raise-gate-min-severity-from-medium-to-high"
        );
        assert_eq!(files[0].0, "config/gate.toml");
        assert_eq!(state.created_prs.len(), 1);
        let (_, head, base, title, body) = &state.created_prs[0];
        assert_eq!(head, branch);
        assert_eq!(base, "main");
        assert!(title.starts_with("[meta] "));
        // Evidence rides in the PR body: the numbers are cited.
        assert!(body.contains("40.0%"));
        assert!(body.contains("2 approved / 5 verdicts"));
        assert_eq!(pr.head_branch, *branch);
    }
}
