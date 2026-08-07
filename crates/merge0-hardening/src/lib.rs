//! The hardening pass (PRD §5c): retiring defect classes, not just defects.
//!
//! When a maintenance Work Order's PR merges, this crate evaluates whether
//! the fixed defect class is mechanically preventable and emits a *separate*
//! follow-up PR through the same pipeline — same gate, same inbox, same
//! human-merge rule. A hardening PR never piggybacks on the fix PR
//! (reviewability and revert-independence require separation).
//!
//! **Enforcement hierarchy** (PRD §5c) — always the most deterministic
//! mechanism the defect class supports:
//!
//! 1. [`Mechanism::LintRule`] — CI-enforced, catches human and agent
//!    contributors alike, zero runtime cost.
//! 2. [`Mechanism::RegressionTest`] — when the pattern is behavioral rather
//!    than syntactic.
//! 3. [`Mechanism::IntentAmendment`] — fallback only, for constraints
//!    expressible solely as guidance. Written exclusively through the fenced
//!    machine section of MERGE0.md via [`merge0_context::intent::IntentDoc`];
//!    customer prose is never rewritten (the fence rule, enforced in code).
//!
//! **Targeting** is outcome-memory-driven: recurring Signal fingerprints
//! (same defect class fixed more than once) are top priority
//! ([`find_candidates`] orders them first). Every emitted artifact embeds
//! `origin: report <ulid> fingerprint <fp>` so a future rule-removal PR can
//! be traced to what it re-exposes (PRD §5c design rule).
//!
//! **Effectiveness is measured, not assumed** ([`effectiveness`]): a merged
//! hardening PR whose fingerprint recurs is a hard negative in outcome
//! memory ([`record_hard_negative`]).
//!
//! Discipline note (PRD §5d): everything this crate produces is a
//! supplemental, git-visible, human-merged artifact. It never modifies its
//! own executor.

use chrono::{DateTime, Utc};
use merge0_context::intent::{FenceError, IntentDoc};
use merge0_github::{GitHubApi, GitHubError, PrInfo, RepoRef};
use merge0_signal::{
    EvidenceKind, EvidenceLink, OutcomeKind, Report, ReportKind, ReportStatus, Signal, SignalKind,
};
use merge0_store::{StoreError, TenantStore};
use std::collections::BTreeSet;
use ulid::Ulid;

#[derive(Debug, thiserror::Error)]
pub enum HardeningError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("github error: {0}")]
    GitHub(#[from] GitHubError),
    /// An intent-doc amendment was requested but no fenced machine section
    /// exists to write into. The fence rule (PRD §5c) forbids writing
    /// anywhere else, so the amendment is refused rather than improvised.
    #[error("intent doc has no machine-managed fence to write into")]
    NoFence,
    #[error("intent doc fence is malformed: {0}")]
    MalformedFence(FenceError),
    #[error("no stored signal for fingerprint {0:?}")]
    MissingSignal(String),
}

/// The prevention mechanism chosen for a defect class, ordered by the
/// enforcement hierarchy: lint > test > intent-doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mechanism {
    /// A CI-enforced lint/AST rule (the most deterministic level).
    LintRule {
        tool: String,
        rule_file_path: String,
        rule_content: String,
    },
    /// A regression-test skeleton (behavioral patterns).
    RegressionTest {
        test_file_path: String,
        test_content: String,
    },
    /// One line appended to MERGE0.md's fenced machine section (fallback
    /// only — an ever-growing "don't do X" list is context rot).
    IntentAmendment { line: String },
}

impl Mechanism {
    /// Human-readable hierarchy level, used in PR bodies.
    fn describe(&self) -> (&'static str, String, &'static str) {
        match self {
            Mechanism::LintRule {
                tool,
                rule_file_path,
                ..
            } => (
                "lint rule",
                format!("{tool} rule at `{rule_file_path}`"),
                "level 1 of the enforcement hierarchy: the pattern is \
                 syntactic, so a CI-enforced rule catches human and agent \
                 contributors alike at zero runtime cost",
            ),
            Mechanism::RegressionTest { test_file_path, .. } => (
                "regression test",
                format!("regression test at `{test_file_path}`"),
                "level 2 of the enforcement hierarchy: the pattern is \
                 behavioral rather than syntactic, so a test — not a lint \
                 rule — is the most deterministic mechanism available",
            ),
            Mechanism::IntentAmendment { line } => (
                "intent-doc amendment",
                format!("MERGE0.md machine-section line: `{line}`"),
                "level 3 (fallback only): the constraint is expressible \
                 solely as guidance, so it lands in the fenced machine \
                 section of the intent doc",
            ),
        }
    }
}

/// A merged maintenance fix whose defect class may be mechanically
/// preventable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardeningCandidate {
    pub origin_report_id: Ulid,
    pub fingerprint: String,
    /// Number of distinct reports containing this fingerprint — the
    /// recurrence signal that drives queue priority.
    pub recurrence_count: usize,
    pub title: String,
}

/// What [`propose`] produced: the follow-up PR plus the Hardening report
/// now sitting in the inbox.
#[derive(Debug, Clone, PartialEq)]
pub struct HardeningProposal {
    /// Id of the `ReportKind::Hardening` report inserted into the store.
    pub report_id: Ulid,
    pub branch: String,
    pub pr: PrInfo,
}

/// Post-merge recurrence check for a hardened fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct Effectiveness {
    pub fingerprint: String,
    /// The fingerprint's signal was seen again after the hardening PR
    /// merged — the mechanism did not prevent the defect class.
    pub recurred: bool,
    pub last_seen: Option<DateTime<Utc>>,
    pub checked_at: DateTime<Utc>,
}

/// Find merged maintenance fixes eligible for a hardening follow-up.
///
/// A candidate is a fingerprint of a maintenance-kind report with a
/// `Merged` outcome, excluding fingerprints that already have a Hardening
/// report (no duplicate hardening PRs). Ordering: recurring fingerprints
/// (`recurrence_count >= 2`) first — PRD §5c makes recurrence top
/// priority — then single-occurrence fixes.
pub async fn find_candidates(
    store: &TenantStore,
) -> Result<Vec<HardeningCandidate>, HardeningError> {
    let reports = store.list_reports(None).await?;

    // Fingerprints already covered by a hardening report, whatever its
    // status: one hardening proposal per defect class.
    let hardened: BTreeSet<&String> = reports
        .iter()
        .filter(|r| r.kind == ReportKind::Hardening)
        .flat_map(|r| r.fingerprints.iter())
        .collect();

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut candidates = Vec::new();
    // `list_reports` orders created_at DESC, so the origin report for a
    // recurring fingerprint is its most recent merged fix.
    for report in reports.iter().filter(|r| r.kind == ReportKind::Maintenance) {
        let outcomes = store.outcomes_for_report(report.id).await?;
        if !outcomes.iter().any(|o| o.outcome == OutcomeKind::Merged) {
            continue;
        }
        for fingerprint in &report.fingerprints {
            if hardened.contains(fingerprint) || !seen.insert(fingerprint.clone()) {
                continue;
            }
            let recurrence_count = store
                .reports_containing_fingerprint(fingerprint)
                .await?
                .len();
            candidates.push(HardeningCandidate {
                origin_report_id: report.id,
                fingerprint: fingerprint.clone(),
                recurrence_count,
                title: report.title.clone(),
            });
        }
    }
    // Stable sort: recurring defect classes first, ties keep recency order.
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.recurrence_count));
    Ok(candidates)
}

/// Substring patterns that mark a defect class as syntactically lintable.
/// Matched case-sensitively against the signal's title + body; the JS-style
/// messages are vendor-stable exception texts, `unwrap` is the Rust panic
/// class.
const LINTABLE_PATTERNS: &[&str] = &[
    "unwrap",
    "Cannot read properties of undefined",
    "undefined is not a function",
    "TypeError",
];

/// Choose the most deterministic mechanism the defect class supports.
///
/// Deterministic heuristic (documented, not learned):
///
/// 1. Signal title/body contains one of [`LINTABLE_PATTERNS`] → a lint rule
///    file under `.merge0/rules/` referencing the pattern (ast-grep-style
///    YAML — CI-enforceable, the top of the hierarchy).
/// 2. Otherwise, `SignalKind::Exception` → a regression-test skeleton under
///    `tests/merge0_regressions/` with the repro preserved in a comment.
///    The skeleton is language-neutral markdown because `synthesize` cannot
///    know the customer's test framework; the human review that merges the
///    hardening PR converts it (or the runner does, in a later phase).
/// 3. Otherwise → an intent-doc amendment line (fallback only).
///
/// Every generated artifact embeds `origin: report <ulid> fingerprint <fp>`
/// as a comment so a future rule-removal PR can be traced to what it
/// re-exposes (PRD §5c design rule).
pub fn synthesize(candidate: &HardeningCandidate, signal: &Signal) -> Mechanism {
    let origin = origin_line(candidate);
    let suffix = fingerprint_suffix(&candidate.fingerprint);
    let haystack = format!("{}\n{}", signal.title, signal.body);

    if let Some(pattern) = LINTABLE_PATTERNS.iter().find(|p| haystack.contains(**p)) {
        let rule_id = format!("merge0-hardening-{suffix}");
        let rule_content = format!(
            "# {origin}\n\
             # Generated by the Merge0 hardening pass (PRD \u{a7}5c). Removing this rule\n\
             # re-exposes the defect class fixed by the report above.\n\
             id: {rule_id}\n\
             language: any\n\
             severity: error\n\
             message: defect class {fp} (pattern `{pattern}`) — see origin report\n\
             rule:\n\
             \x20 pattern: \"{pattern}\"\n",
            fp = candidate.fingerprint,
        );
        return Mechanism::LintRule {
            tool: "ast-grep".to_string(),
            rule_file_path: format!(".merge0/rules/{rule_id}.yml"),
            rule_content,
        };
    }

    if signal.kind == SignalKind::Exception {
        // `-->` inside the repro would terminate the HTML comment early.
        let repro = signal.body.replace("-->", "-- >");
        let test_content = format!(
            "<!-- {origin} -->\n\
             # Regression test skeleton: {title}\n\n\
             Generated by the Merge0 hardening pass (PRD \u{a7}5c). Convert this skeleton\n\
             into an executable test in this repository's test framework before or\n\
             during review; removing it re-exposes the defect class fixed by the\n\
             origin report.\n\n\
             ## Repro\n\n\
             <!-- repro (verbatim from the originating signal):\n{repro}\n-->\n\n\
             ## Expected\n\n\
             The defect described above does not recur.\n",
            title = single_line(&candidate.title),
        );
        return Mechanism::RegressionTest {
            test_file_path: format!("tests/merge0_regressions/regression_{suffix}.md"),
            test_content,
        };
    }

    Mechanism::IntentAmendment {
        line: format!(
            "- [hardening] {}: prevented defect class; do not reintroduce ({origin})",
            single_line(&candidate.title),
        ),
    }
}

/// Open the hardening follow-up PR and file the matching Hardening report.
///
/// Branch `merge0/hardening-{fingerprint-suffix}` carries only the
/// prevention artifact — never the fix itself. For
/// [`Mechanism::IntentAmendment`] the written file is MERGE0.md produced
/// exclusively via [`IntentDoc::append_machine_line`] on
/// `existing_intent_doc`; a missing doc or missing fence is
/// [`HardeningError::NoFence`] (the fence rule is enforced here, not left
/// to prompt discipline). A `ReportKind::Hardening` report (status
/// `AwaitingReview`, fingerprint + origin signal attached, PR link as
/// evidence) is inserted so the proposal shows up in the inbox.
pub async fn propose(
    candidate: &HardeningCandidate,
    mechanism: &Mechanism,
    api: &dyn GitHubApi,
    repo: &RepoRef,
    store: &TenantStore,
    existing_intent_doc: Option<&str>,
    now: DateTime<Utc>,
) -> Result<HardeningProposal, HardeningError> {
    let signal = store
        .signal_by_fingerprint(&candidate.fingerprint)
        .await?
        .ok_or_else(|| HardeningError::MissingSignal(candidate.fingerprint.clone()))?;

    let files = mechanism_files(mechanism, existing_intent_doc)?;
    let branch = format!(
        "merge0/hardening-{}",
        fingerprint_suffix(&candidate.fingerprint)
    );
    let title = format!("[hardening] {}", single_line(&candidate.title));
    let body = pr_body(candidate, mechanism, &signal);

    let base = api.default_branch(repo).await?;
    api.create_branch_with_files(repo, &branch, &files, &title)
        .await?;
    let pr = api
        .create_pull_request(repo, &branch, &base, &title, &body)
        .await?;

    let (level, _, _) = mechanism.describe();
    let report = Report {
        id: Ulid::new(),
        kind: ReportKind::Hardening,
        title,
        summary: format!(
            "Prevention follow-up ({level}) for merged fix {origin}; recurrence \
             count {count}. See the hardening PR for the artifact and evidence.",
            origin = candidate.origin_report_id,
            count = candidate.recurrence_count,
        ),
        severity: signal.severity,
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: "Hardening PR".to_string(),
            url: pr.url.clone(),
        }],
        signal_ids: vec![signal.id],
        fingerprints: vec![candidate.fingerprint.clone()],
        suspect_release: None,
        affected_count: None,
        status: ReportStatus::AwaitingReview,
        created_at: now,
    };
    store.insert_report(&report).await?;

    Ok(HardeningProposal {
        report_id: report.id,
        branch,
        pr,
    })
}

/// Did the hardened fingerprint recur after the hardening PR merged?
///
/// Recurrence = the fingerprint's stored signal has `last_seen` after
/// `hardening_merged_at`. A recurrence is a hard negative — the caller
/// records it via [`record_hard_negative`].
pub async fn effectiveness(
    store: &TenantStore,
    fingerprint: &str,
    hardening_merged_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Effectiveness, HardeningError> {
    let last_seen = store
        .signal_by_fingerprint(fingerprint)
        .await?
        .map(|s| s.last_seen);
    Ok(Effectiveness {
        fingerprint: fingerprint.to_string(),
        recurred: last_seen.is_some_and(|seen| seen > hardening_merged_at),
        last_seen,
        checked_at: now,
    })
}

/// Write the hard negative into outcome memory (PRD §5c: "a merged
/// hardening PR whose fingerprint recurs is a hard negative"): a
/// `Reverted`-kind outcome on the hardening report.
pub async fn record_hard_negative(
    store: &TenantStore,
    report_id: Ulid,
    now: DateTime<Utc>,
) -> Result<(), HardeningError> {
    store
        .record_outcome(
            report_id,
            OutcomeKind::Reverted,
            None,
            now,
            Some("hardening ineffective: fingerprint recurred"),
            None,
        )
        .await?;
    Ok(())
}

// ---- internals ----

/// The traceability comment every artifact must carry (PRD §5c).
fn origin_line(candidate: &HardeningCandidate) -> String {
    format!(
        "origin: report {} fingerprint {}",
        candidate.origin_report_id, candidate.fingerprint
    )
}

/// Branch-safe short form of a fingerprint (`source:hex` → first 12 hex
/// chars). Falls back to filtering the whole string if the shape is
/// unexpected — never panics on untrusted input.
fn fingerprint_suffix(fingerprint: &str) -> String {
    let hex = fingerprint
        .rsplit(':')
        .next()
        .unwrap_or(fingerprint)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>();
    if hex.is_empty() {
        "unknown".to_string()
    } else {
        hex
    }
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The files the hardening branch carries, per mechanism. The intent-doc
/// path goes through [`IntentDoc`] exclusively — the fence rule enforcement.
fn mechanism_files(
    mechanism: &Mechanism,
    existing_intent_doc: Option<&str>,
) -> Result<Vec<(String, String)>, HardeningError> {
    match mechanism {
        Mechanism::LintRule {
            rule_file_path,
            rule_content,
            ..
        } => Ok(vec![(rule_file_path.clone(), rule_content.clone())]),
        Mechanism::RegressionTest {
            test_file_path,
            test_content,
        } => Ok(vec![(test_file_path.clone(), test_content.clone())]),
        Mechanism::IntentAmendment { line } => {
            let doc = existing_intent_doc.ok_or(HardeningError::NoFence)?;
            let parsed = IntentDoc::parse(doc).map_err(|e| match e {
                FenceError::MissingFence => HardeningError::NoFence,
                FenceError::MalformedFence => HardeningError::MalformedFence(e),
            })?;
            let amended = parsed.append_machine_line(line);
            Ok(vec![(
                "MERGE0.md".to_string(),
                amended.full_text().to_string(),
            )])
        }
    }
}

/// The reviewer-facing PR body: defect class, origin, mechanism, hierarchy
/// justification, evidence — decision-ready from the PR page alone (PRD
/// §6a).
fn pr_body(candidate: &HardeningCandidate, mechanism: &Mechanism, signal: &Signal) -> String {
    let (level, artifact, why) = mechanism.describe();
    let recurrence = if candidate.recurrence_count >= 2 {
        format!(
            "{} distinct reports contain this fingerprint — a recurring defect \
             class (top hardening priority)",
            candidate.recurrence_count
        )
    } else {
        "single occurrence; the rule is derivable from the fixed defect".to_string()
    };
    format!(
        "## Hardening follow-up (PRD \u{a7}5c)\n\n\
         **Defect class:** {title}\n\
         **Origin report:** {origin_id}\n\
         **Fingerprint:** `{fp}`\n\
         **Mechanism:** {level} — {artifact}\n\
         **Why this level of the hierarchy:** {why}.\n\n\
         **Evidence:** originating signal `{source_ref}` (severity \
         {severity:?}, last seen {last_seen}); {recurrence}.\n\n\
         This PR is a separate prevention follow-up to an already-merged fix; \
         it deliberately never piggybacks on the fix PR. The artifact embeds \
         `{origin}` so removing it later can be traced to what it re-exposes.\n",
        title = single_line(&candidate.title),
        origin_id = candidate.origin_report_id,
        fp = candidate.fingerprint,
        source_ref = signal.source_ref,
        severity = signal.severity,
        last_seen = signal.last_seen.to_rfc3339(),
        origin = origin_line(candidate),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_context::intent::{FENCE_END, FENCE_START, MERGE0_TEMPLATE};
    use merge0_signal::{fingerprint, JoinKeys, Severity, Source};

    fn candidate(fp: &str) -> HardeningCandidate {
        HardeningCandidate {
            origin_report_id: Ulid::from_string("01J0000000000000000000TEST").unwrap(),
            fingerprint: fp.to_string(),
            recurrence_count: 1,
            title: "Null district crash in SyncStatusPanel".to_string(),
        }
    }

    fn signal(kind: SignalKind, title: &str, body: &str, fp: &str) -> Signal {
        Signal {
            id: Ulid::new(),
            source: Source::Sentry,
            source_ref: "s1".to_string(),
            kind,
            severity: Severity::High,
            title: title.to_string(),
            body: body.to_string(),
            evidence: vec![],
            fingerprint: fp.to_string(),
            join_keys: JoinKeys::default(),
            affected_count: None,
            first_seen: chrono::DateTime::UNIX_EPOCH,
            last_seen: chrono::DateTime::UNIX_EPOCH,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn lintable_pattern_wins_over_exception_kind() {
        // Exception AND lintable: the hierarchy prefers the lint rule.
        let fp = fingerprint(Source::Sentry, &["issue", "1"]);
        let c = candidate(&fp);
        let s = signal(
            SignalKind::Exception,
            "TypeError: Cannot read properties of undefined",
            "boom",
            &fp,
        );
        match synthesize(&c, &s) {
            Mechanism::LintRule {
                tool,
                rule_file_path,
                rule_content,
            } => {
                assert_eq!(tool, "ast-grep");
                assert!(rule_file_path.starts_with(".merge0/rules/"));
                assert!(rule_file_path.ends_with(".yml"));
                // The rule references the matched pattern (first in the
                // pattern table order that matches).
                assert!(rule_content.contains("Cannot read properties of undefined"));
            }
            other => panic!("expected LintRule, got {other:?}"),
        }
    }

    #[test]
    fn exception_without_lintable_pattern_gets_regression_test() {
        let fp = fingerprint(Source::Sentry, &["issue", "2"]);
        let c = candidate(&fp);
        let s = signal(
            SignalKind::Exception,
            "Panel crashes on empty roster",
            "Open /districts/sync for a school with no linked district",
            &fp,
        );
        match synthesize(&c, &s) {
            Mechanism::RegressionTest {
                test_file_path,
                test_content,
            } => {
                assert!(test_file_path.starts_with("tests/merge0_regressions/"));
                // Repro rides along in a comment.
                assert!(test_content.contains("no linked district"));
                assert!(test_content.contains("<!-- repro"));
            }
            other => panic!("expected RegressionTest, got {other:?}"),
        }
    }

    #[test]
    fn non_exception_non_lintable_falls_back_to_intent_amendment() {
        let fp = fingerprint(Source::Zendesk, &["ticket", "3"]);
        let c = candidate(&fp);
        let s = signal(
            SignalKind::Ticket,
            "Export confusion",
            "user was confused",
            &fp,
        );
        match synthesize(&c, &s) {
            Mechanism::IntentAmendment { line } => {
                assert!(line.starts_with("- "));
                assert!(!line.contains('\n'), "amendment must be a single line");
            }
            other => panic!("expected IntentAmendment, got {other:?}"),
        }
    }

    #[test]
    fn every_mechanism_embeds_origin_report_and_fingerprint() {
        // PRD §5c design rule: a future rule-removal PR must be traceable to
        // what it re-exposes.
        let fp = fingerprint(Source::Sentry, &["issue", "4"]);
        let c = candidate(&fp);
        let expected = format!("origin: report {} fingerprint {fp}", c.origin_report_id);
        let cases = [
            signal(SignalKind::Exception, "TypeError in panel", "boom", &fp),
            signal(
                SignalKind::Exception,
                "Crash on empty roster",
                "repro steps",
                &fp,
            ),
            signal(SignalKind::Ticket, "Export confusion", "confused", &fp),
        ];
        for s in &cases {
            let content = match synthesize(&c, s) {
                Mechanism::LintRule { rule_content, .. } => rule_content,
                Mechanism::RegressionTest { test_content, .. } => test_content,
                Mechanism::IntentAmendment { line } => line,
            };
            assert!(
                content.contains(&expected),
                "artifact for {:?} must embed {expected:?}, got:\n{content}",
                s.kind
            );
        }
    }

    #[test]
    fn intent_amendment_preserves_human_prose_byte_for_byte() {
        let original = format!(
            "# Chalk intent\n\n- precious human prose \u{2014} do not touch\n\n\
             {FENCE_START}\n{FENCE_END}\n\n## More human prose\n"
        );
        let human_before = IntentDoc::parse(&original).unwrap().human_text();

        let mechanism = Mechanism::IntentAmendment {
            line: "- [hardening] rule (origin: report X fingerprint Y)".to_string(),
        };
        let files = mechanism_files(&mechanism, Some(&original)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "MERGE0.md");

        let after = IntentDoc::parse(&files[0].1).unwrap();
        assert_eq!(
            after.human_text(),
            human_before,
            "human prose must be byte-identical"
        );
        assert_eq!(
            after.machine_section(),
            "- [hardening] rule (origin: report X fingerprint Y)"
        );
    }

    #[test]
    fn intent_amendment_without_fence_is_no_fence_error() {
        let mechanism = Mechanism::IntentAmendment {
            line: "- rule".to_string(),
        };
        assert!(matches!(
            mechanism_files(&mechanism, Some("# doc with no fence")),
            Err(HardeningError::NoFence)
        ));
        assert!(matches!(
            mechanism_files(&mechanism, None),
            Err(HardeningError::NoFence)
        ));
        let malformed = format!("{FENCE_END}\n{FENCE_START}");
        assert!(matches!(
            mechanism_files(&mechanism, Some(&malformed)),
            Err(HardeningError::MalformedFence(_))
        ));
    }

    #[test]
    fn intent_amendment_on_template_lands_inside_fence() {
        let mechanism = Mechanism::IntentAmendment {
            line: "- [hardening] never crash on missing district".to_string(),
        };
        let files = mechanism_files(&mechanism, Some(MERGE0_TEMPLATE)).unwrap();
        let doc = IntentDoc::parse(&files[0].1).unwrap();
        assert!(doc
            .machine_section()
            .contains("never crash on missing district"));
    }

    #[test]
    fn fingerprint_suffix_is_branch_safe() {
        let fp = fingerprint(Source::Sentry, &["issue", "1"]);
        let suffix = fingerprint_suffix(&fp);
        assert_eq!(suffix.len(), 12);
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()));
        // Untrusted shapes never panic and never produce an empty suffix.
        assert_eq!(fingerprint_suffix(""), "unknown");
        assert_eq!(fingerprint_suffix("::::"), "unknown");
        assert_eq!(fingerprint_suffix("weird fp !!"), "weirdfp");
    }

    #[test]
    fn pr_body_is_decision_ready() {
        let fp = fingerprint(Source::Sentry, &["issue", "9"]);
        let mut c = candidate(&fp);
        c.recurrence_count = 3;
        let s = signal(SignalKind::Exception, "TypeError in panel", "boom", &fp);
        let mechanism = synthesize(&c, &s);
        let body = pr_body(&c, &mechanism, &s);
        assert!(body.contains(&c.origin_report_id.to_string()));
        assert!(body.contains(&fp));
        assert!(body.contains("recurring defect class"));
        assert!(body.contains("hierarchy"));
    }
}
