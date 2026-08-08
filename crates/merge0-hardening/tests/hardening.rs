//! Hardening pass against real Postgres + FakeGitHub (PRD §5c): seed a
//! merged maintenance fix, find it, propose the prevention PR, verify the
//! Hardening report and the dedupe, then measure effectiveness.

use chrono::{DateTime, Duration, TimeZone, Utc};
use merge0_github::{FakeGitHub, RepoRef};
use merge0_hardening::{
    effectiveness, find_candidates, propose, record_hard_negative, synthesize, Mechanism,
};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, OutcomeKind, Report, ReportKind,
    ReportStatus, Severity, Signal, SignalKind, Source,
};
use merge0_store::{Store, TenantStore};
use ulid::Ulid;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 7, 0, 0, 0).unwrap()
}

fn exception_signal(reference: &str, title: &str) -> Signal {
    Signal {
        id: Ulid::new(),
        source: Source::Sentry,
        source_ref: reference.to_string(),
        kind: SignalKind::Exception,
        severity: Severity::High,
        title: title.to_string(),
        body: "Open /districts/sync for a school with no linked district".to_string(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: "Sentry issue".to_string(),
            url: format!("https://sentry.example.com/{reference}"),
        }],
        fingerprint: fingerprint(Source::Sentry, &["issue", reference]),
        join_keys: JoinKeys::default(),
        affected_count: Some(12),
        delegated: false,
        first_seen: now() - Duration::days(3),
        last_seen: now() - Duration::hours(2),
        raw: serde_json::Value::Null,
    }
}

fn maintenance_report(signal: &Signal, created_at: DateTime<Utc>) -> Report {
    Report {
        id: Ulid::new(),
        kind: ReportKind::Maintenance,
        title: signal.title.clone(),
        summary: "seeded".to_string(),
        severity: signal.severity,
        evidence: signal.evidence.clone(),
        signal_ids: vec![signal.id],
        fingerprints: vec![signal.fingerprint.clone()],
        suspect_release: None,
        affected_count: signal.affected_count,
        status: ReportStatus::Completed,
        created_at,
    }
}

/// Seed the pipeline.rs pattern: signal → report → dispatch → PR → merged.
async fn seed_merged_fix(tenant: &TenantStore, signal: &Signal, created_at: DateTime<Utc>) -> Ulid {
    tenant.upsert_signal(signal).await.unwrap();
    let report = maintenance_report(signal, created_at);
    tenant.insert_report(&report).await.unwrap();
    tenant
        .record_dispatch(report.id, "github-actions", created_at)
        .await
        .unwrap();
    tenant
        .record_pr_opened(
            report.id,
            &format!("https://github.com/acme/chalk/pull/{}", report.id),
            "merge0/fix",
            created_at + Duration::hours(1),
            Some(50_000),
            None,
        )
        .await
        .unwrap();
    tenant
        .record_outcome(
            report.id,
            OutcomeKind::Merged,
            None,
            created_at + Duration::hours(5),
            None,
            Some(50_000),
        )
        .await
        .unwrap();
    report.id
}

#[tokio::test]
async fn merged_fix_produces_hardening_pr_report_and_dedupes() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    let signal = exception_signal("s1", "TypeError: Cannot read properties of undefined");
    let origin_id = seed_merged_fix(&tenant, &signal, now() - Duration::days(1)).await;

    // A merged maintenance fix is a candidate exactly once.
    let candidates = find_candidates(&tenant).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.origin_report_id, origin_id);
    assert_eq!(candidate.fingerprint, signal.fingerprint);
    assert_eq!(candidate.recurrence_count, 1);

    // TypeError is lintable → top of the hierarchy.
    let mechanism = synthesize(candidate, &signal);
    assert!(matches!(mechanism, Mechanism::LintRule { .. }));

    let api = FakeGitHub::new();
    let repo = RepoRef::parse("acme/chalk").unwrap();
    let proposal = propose(candidate, &mechanism, &api, &repo, &tenant, None, now())
        .await
        .unwrap();

    // Branch + PR created with the expected shape. (Scoped so the fake's
    // lock guard is released before the next await.)
    {
        let state = api.state.lock().unwrap();
        assert_eq!(state.created_branches.len(), 1);
        let (branch_repo, branch, files, _) = &state.created_branches[0];
        assert_eq!(branch_repo, &repo);
        assert!(branch.starts_with("merge0/hardening-"));
        assert_eq!(branch, &proposal.branch);
        assert_eq!(files.len(), 1);
        assert!(files[0].0.starts_with(".merge0/rules/"));
        let expected_origin = format!(
            "origin: report {origin_id} fingerprint {}",
            signal.fingerprint
        );
        assert!(
            files[0].1.contains(&expected_origin),
            "rule file must embed the origin comment"
        );
        assert_eq!(state.created_prs.len(), 1);
        let (_, head, base, title, body) = &state.created_prs[0];
        assert_eq!(head, branch);
        assert_eq!(base, "main");
        assert!(title.starts_with("[hardening]"));
        assert!(body.contains(&origin_id.to_string()));
        assert!(body.contains(&signal.fingerprint));
    }

    // The Hardening report is in the inbox, linked to signal + fingerprint
    // + PR evidence.
    let hardening_report = tenant.get_report(proposal.report_id).await.unwrap();
    assert_eq!(hardening_report.kind, ReportKind::Hardening);
    assert_eq!(hardening_report.status, ReportStatus::AwaitingReview);
    assert_eq!(
        hardening_report.fingerprints,
        vec![signal.fingerprint.clone()]
    );
    assert_eq!(hardening_report.signal_ids, vec![signal.id]);
    assert_eq!(hardening_report.evidence.len(), 1);
    assert_eq!(hardening_report.evidence[0].url, proposal.pr.url);

    // Dedupe: the fingerprint now has a hardening report → no candidates.
    let candidates = find_candidates(&tenant).await.unwrap();
    assert!(candidates.is_empty(), "no duplicate hardening PRs");

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn recurring_fingerprints_order_before_single_occurrence() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    // fp A: fixed twice (two merged reports); fp B: fixed once, more
    // recently. Recurrence outranks recency.
    let recurring = exception_signal("recurring", "TypeError in sync panel");
    seed_merged_fix(&tenant, &recurring, now() - Duration::days(10)).await;
    let second_fix = seed_merged_fix(&tenant, &recurring, now() - Duration::days(5)).await;
    let single = exception_signal("single", "Crash on empty roster");
    seed_merged_fix(&tenant, &single, now() - Duration::days(1)).await;

    let candidates = find_candidates(&tenant).await.unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].fingerprint, recurring.fingerprint);
    assert_eq!(candidates[0].recurrence_count, 2);
    // Origin is the most recent merged fix of the recurring class.
    assert_eq!(candidates[0].origin_report_id, second_fix);
    assert_eq!(candidates[1].fingerprint, single.fingerprint);
    assert_eq!(candidates[1].recurrence_count, 1);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn unmerged_reports_are_not_candidates() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    // Report exists, PR opened, but outcome is Closed — not hardening bait.
    let signal = exception_signal("closed", "TypeError somewhere");
    tenant.upsert_signal(&signal).await.unwrap();
    let report = maintenance_report(&signal, now() - Duration::days(1));
    tenant.insert_report(&report).await.unwrap();
    tenant
        .record_dispatch(report.id, "github-actions", now() - Duration::days(1))
        .await
        .unwrap();
    tenant
        .record_outcome(report.id, OutcomeKind::Closed, None, now(), None, None)
        .await
        .unwrap();

    assert!(find_candidates(&tenant).await.unwrap().is_empty());

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn recurrence_after_merge_is_a_hard_negative() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    let signal = exception_signal("recur", "TypeError again");
    seed_merged_fix(&tenant, &signal, now() - Duration::days(2)).await;

    let candidate = &find_candidates(&tenant).await.unwrap()[0];
    let mechanism = synthesize(candidate, &signal);
    let api = FakeGitHub::new();
    let repo = RepoRef::parse("acme/chalk").unwrap();
    let proposal = propose(candidate, &mechanism, &api, &repo, &tenant, None, now())
        .await
        .unwrap();

    // Hardening PR merges now; signal last_seen is before that → quiet.
    let merged_at = now();
    let check = effectiveness(&tenant, &signal.fingerprint, merged_at, now())
        .await
        .unwrap();
    assert!(!check.recurred);

    // The fingerprint recurs after the merge → hard negative.
    let recurrence = Signal {
        id: Ulid::new(),
        last_seen: merged_at + Duration::days(1),
        ..signal.clone()
    };
    tenant.upsert_signal(&recurrence).await.unwrap();
    let check = effectiveness(
        &tenant,
        &signal.fingerprint,
        merged_at,
        merged_at + Duration::days(2),
    )
    .await
    .unwrap();
    assert!(check.recurred);
    assert_eq!(check.last_seen, Some(merged_at + Duration::days(1)));

    record_hard_negative(&tenant, proposal.report_id, merged_at + Duration::days(2))
        .await
        .unwrap();
    let outcomes = tenant
        .outcomes_for_report(proposal.report_id)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome, OutcomeKind::Reverted);
    assert_eq!(
        outcomes[0].note.as_deref(),
        Some("hardening ineffective: fingerprint recurred")
    );

    // An unknown fingerprint never recurred (and never panics).
    let check = effectiveness(&tenant, "sentry:doesnotexist", now(), now())
        .await
        .unwrap();
    assert!(!check.recurred);
    assert_eq!(check.last_seen, None);

    store.drop_tenant(&schema).await.unwrap();
}
