//! Integration tests against a real Postgres (see README: a local cluster on
//! port 55432, or set MERGE0_TEST_DATABASE_URL). Each test provisions its own
//! throwaway tenant schema, exercising schema-per-tenant isolation for real,
//! and drops it on the way out.

use chrono::{DateTime, Duration, TimeZone, Utc};
use merge0_signal::{
    fingerprint, DismissReason, EvidenceKind, EvidenceLink, GateDecision, JoinKeys, OutcomeKind,
    Report, ReportKind, ReportStatus, Severity, Signal, SignalKind, Source, WorkOrder,
};
use merge0_store::{IngestOutcome, Store, StoreError, TenantStore};
use ulid::Ulid;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

async fn fresh_tenant() -> (Store, TenantStore, String) {
    let store = Store::connect(&database_url())
        .await
        .expect("test Postgres must be reachable — see README (Testing)");
    let schema = format!("t_{}", Ulid::generate().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.expect("provision tenant");
    (store, tenant, schema)
}

fn ts(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
}

fn sample_signal(fp_part: &str, first: DateTime<Utc>, last: DateTime<Utc>) -> Signal {
    Signal {
        id: Ulid::generate(),
        source: Source::Sentry,
        source_ref: fp_part.to_string(),
        kind: SignalKind::Exception,
        severity: Severity::High,
        title: format!("TypeError in {fp_part}"),
        body: "boom".into(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: "Sentry issue".into(),
            url: format!("https://sentry.example.com/issues/{fp_part}/"),
        }],
        fingerprint: fingerprint(Source::Sentry, &["issue", fp_part]),
        join_keys: JoinKeys {
            release: Some("v2.3.0".into()),
            ..Default::default()
        },
        affected_count: Some(10),
        delegated: false,
        first_seen: first,
        last_seen: last,
        raw: serde_json::json!({"id": fp_part}),
    }
}

fn sample_report(signals: &[&Signal], kind: ReportKind) -> Report {
    Report {
        id: Ulid::generate(),
        kind,
        title: "Crash cluster".into(),
        summary: "42 users affected".into(),
        severity: Severity::High,
        evidence: vec![],
        signal_ids: signals.iter().map(|s| s.id).collect(),
        fingerprints: signals.iter().map(|s| s.fingerprint.clone()).collect(),
        suspect_release: Some("v2.3.0".into()),
        affected_count: Some(42),
        status: ReportStatus::Pending,
        created_at: ts(6, 0),
    }
}

fn sample_work_order(report: &Report) -> WorkOrder {
    WorkOrder {
        report_id: report.id,
        repo: "chalk/chalk".into(),
        summary: "Fix crash".into(),
        evidence: vec![],
        repro: "open /districts/sync".into(),
        suspect_change: None,
        success_criteria: "regression test passes".into(),
        constraints: "".into(),
        prior_attempts: vec![],
        diff_budget: Default::default(),
        confidence: Default::default(),
    }
}

#[tokio::test]
async fn provision_is_idempotent_and_tenants_are_isolated() {
    let (store, tenant_a, schema_a) = fresh_tenant().await;
    // Re-opening the same tenant must be a no-op, not an error.
    store.tenant(&schema_a).await.expect("idempotent provision");

    let schema_b = format!("t_{}", Ulid::generate().to_string().to_lowercase());
    let tenant_b = store.tenant(&schema_b).await.unwrap();

    let signal = sample_signal("iso-1", ts(1, 0), ts(2, 0));
    tenant_a.upsert_signal(&signal).await.unwrap();
    assert_eq!(tenant_a.signals_since(ts(1, 0)).await.unwrap().len(), 1);
    assert_eq!(
        tenant_b.signals_since(ts(1, 0)).await.unwrap().len(),
        0,
        "tenant B must not see tenant A's signals"
    );

    store.drop_tenant(&schema_a).await.unwrap();
    store.drop_tenant(&schema_b).await.unwrap();
}

#[tokio::test]
async fn invalid_schema_names_are_rejected_before_sql() {
    let store = Store::connect(&database_url()).await.unwrap();
    let err = store.tenant("bad\"; DROP SCHEMA public; --").await;
    assert!(matches!(err, Err(StoreError::InvalidSchemaName(_))));
}

#[tokio::test]
async fn signal_upsert_dedupes_on_fingerprint_and_merges_windows() {
    let (store, tenant, schema) = fresh_tenant().await;

    let first = sample_signal("dup-1", ts(2, 0), ts(3, 0));
    assert_eq!(
        tenant.upsert_signal(&first).await.unwrap(),
        IngestOutcome::Inserted
    );

    // Same defect re-ingested later: wider window, higher impact.
    let mut refresh = sample_signal("dup-1", ts(4, 0), ts(6, 0));
    refresh.affected_count = Some(99);
    refresh.severity = Severity::Critical;
    assert_eq!(
        tenant.upsert_signal(&refresh).await.unwrap(),
        IngestOutcome::Updated
    );

    let stored = tenant
        .signal_by_fingerprint(&first.fingerprint)
        .await
        .unwrap()
        .expect("signal exists");
    assert_eq!(stored.id, first.id, "row identity is the first ingest");
    assert_eq!(stored.first_seen, ts(2, 0), "first_seen keeps the earliest");
    assert_eq!(stored.last_seen, ts(6, 0), "last_seen takes the latest");
    assert_eq!(stored.affected_count, Some(99));
    assert_eq!(stored.severity, Severity::Critical);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn unassigned_signals_excludes_report_members() {
    let (store, tenant, schema) = fresh_tenant().await;

    let in_report = sample_signal("assigned", ts(1, 0), ts(2, 0));
    let free = sample_signal("free", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&in_report).await.unwrap();
    tenant.upsert_signal(&free).await.unwrap();

    tenant
        .insert_report(&sample_report(&[&in_report], ReportKind::Maintenance))
        .await
        .unwrap();

    let unassigned = tenant.unassigned_signals().await.unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].fingerprint, free.fingerprint);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn report_lifecycle_gate_approve_dispatch_pr_outcome() {
    let (store, tenant, schema) = fresh_tenant().await;

    let signal = sample_signal("life-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let report = sample_report(&[&signal], ReportKind::Maintenance);
    tenant.insert_report(&report).await.unwrap();

    // Round-trip check.
    let loaded = tenant.get_report(report.id).await.unwrap();
    assert_eq!(loaded, report);

    // Gate emits a work order.
    let decision = GateDecision::Work {
        work_order: sample_work_order(&report),
    };
    tenant
        .set_gate_decision(
            report.id,
            &decision,
            Some("=== SYSTEM ===\ngate prompt\n=== PROMPT ===\nreport bundle"),
        )
        .await
        .unwrap();
    assert_eq!(
        tenant.get_report(report.id).await.unwrap().status,
        ReportStatus::AwaitingReview
    );
    assert_eq!(
        tenant.gate_decision(report.id).await.unwrap(),
        Some(decision)
    );
    assert!(tenant.work_order(report.id).await.unwrap().is_some());

    // Human approves; dispatch; PR opens; PR merges.
    tenant.approve_report(report.id, ts(6, 10)).await.unwrap();
    tenant
        .record_dispatch(report.id, "claude-code", ts(6, 11))
        .await
        .unwrap();
    tenant
        .set_report_status(report.id, ReportStatus::Dispatched)
        .await
        .unwrap();
    tenant
        .record_pr_opened(
            report.id,
            "https://github.com/chalk/chalk/pull/99",
            "merge0/fix-life-1",
            ts(6, 12),
            Some(120_000),
            Some(serde_json::json!({"skills": ["house-style"]})),
        )
        .await
        .unwrap();
    tenant
        .set_report_status(report.id, ReportStatus::PrOpen)
        .await
        .unwrap();
    tenant
        .record_outcome(
            report.id,
            OutcomeKind::Merged,
            Some("https://github.com/chalk/chalk/pull/99"),
            ts(6, 20),
            None,
            Some(120_000),
        )
        .await
        .unwrap();
    tenant
        .set_report_status(report.id, ReportStatus::Completed)
        .await
        .unwrap();

    // PR → report lookup used by the webhook handler.
    assert_eq!(
        tenant
            .report_for_pr("https://github.com/chalk/chalk/pull/99")
            .await
            .unwrap(),
        Some(report.id)
    );

    // Dispatch record captured attribution and spend.
    let dispatch = tenant.dispatch(report.id).await.unwrap().unwrap();
    assert_eq!(dispatch.pr_opened_at, Some(ts(6, 12)));
    assert_eq!(dispatch.tokens_spent, Some(120_000));
    assert_eq!(
        dispatch.extensions.unwrap()["skills"][0],
        serde_json::json!("house-style")
    );

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn gate_skip_and_dismissal_paths() {
    let (store, tenant, schema) = fresh_tenant().await;

    let s1 = sample_signal("skip-1", ts(1, 0), ts(2, 0));
    let s2 = sample_signal("dis-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&s1).await.unwrap();
    tenant.upsert_signal(&s2).await.unwrap();

    let skipped = sample_report(&[&s1], ReportKind::Maintenance);
    tenant.insert_report(&skipped).await.unwrap();
    tenant
        .set_gate_decision(
            skipped.id,
            &GateDecision::Skip {
                reason: "no testable success criterion".into(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        tenant.get_report(skipped.id).await.unwrap().status,
        ReportStatus::Skipped
    );
    assert!(tenant.work_order(skipped.id).await.unwrap().is_none());

    let dismissed = sample_report(&[&s2], ReportKind::Maintenance);
    tenant.insert_report(&dismissed).await.unwrap();
    tenant
        .dismiss_report(dismissed.id, DismissReason::IntendedBehavior, ts(6, 5))
        .await
        .unwrap();
    assert_eq!(
        tenant.get_report(dismissed.id).await.unwrap().status,
        ReportStatus::Dismissed
    );

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn outcome_memory_by_fingerprint_records_reverts_as_history() {
    let (store, tenant, schema) = fresh_tenant().await;

    let signal = sample_signal("prior-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let march_report = sample_report(&[&signal], ReportKind::Maintenance);
    tenant.insert_report(&march_report).await.unwrap();
    tenant
        .record_dispatch(march_report.id, "claude-code", ts(3, 0))
        .await
        .unwrap();
    tenant
        .record_outcome(
            march_report.id,
            OutcomeKind::Reverted,
            Some("https://github.com/chalk/chalk/pull/12"),
            ts(4, 0),
            Some("reverted: broke admin view"),
            None,
        )
        .await
        .unwrap();

    let history = tenant
        .outcomes_for_fingerprint(&signal.fingerprint)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome, OutcomeKind::Reverted);
    assert_eq!(history[0].work_order_id, march_report.id);
    assert_eq!(
        history[0].note.as_deref(),
        Some("reverted: broke admin view")
    );
    // Schema v0.5: the attempt's PR must survive the round-trip. It was
    // written from the start and silently dropped by this query, so the gate
    // knew a fix had been reverted but never what it changed — which can
    // justify declining and can never justify a better attempt.
    assert_eq!(
        history[0].pr_url.as_deref(),
        Some("https://github.com/chalk/chalk/pull/12"),
        "outcome memory must carry the attempt's PR, not just its verdict"
    );

    // Recurrence query for hardening targeting.
    let recurring = tenant
        .reports_containing_fingerprint(&signal.fingerprint)
        .await
        .unwrap();
    assert_eq!(recurring.len(), 1);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn discard_records_salvage_diagnosis_and_outcome() {
    let (store, tenant, schema) = fresh_tenant().await;

    let signal = sample_signal("disc-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let report = sample_report(&[&signal], ReportKind::Maintenance);
    tenant.insert_report(&report).await.unwrap();
    tenant
        .record_dispatch(report.id, "claude-code", ts(3, 0))
        .await
        .unwrap();
    tenant
        .record_discard(
            report.id,
            "diff budget exceeded",
            "fix requires touching the scheduler and 9 files; defect looks architectural",
            ts(3, 2),
            Some(80_000),
        )
        .await
        .unwrap();

    let dispatch = tenant.dispatch(report.id).await.unwrap().unwrap();
    assert_eq!(
        dispatch.discard_reason.as_deref(),
        Some("diff budget exceeded")
    );
    assert!(dispatch.diagnosis.unwrap().contains("architectural"));

    let outcomes = tenant.outcomes_for_report(report.id).await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome, OutcomeKind::Discarded);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn opportunity_reports_hand_off_with_brief() {
    let (store, tenant, schema) = fresh_tenant().await;

    let signal = sample_signal("opp-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let report = sample_report(&[&signal], ReportKind::Opportunity);
    tenant.insert_report(&report).await.unwrap();
    tenant
        .hand_off_report(
            report.id,
            "Users keep colliding with the sync design",
            ts(6, 0),
        )
        .await
        .unwrap();

    assert_eq!(
        tenant.get_report(report.id).await.unwrap().status,
        ReportStatus::HandedOff
    );
    assert_eq!(
        tenant.handoff_brief(report.id).await.unwrap().as_deref(),
        Some("Users keep colliding with the sync design")
    );

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn outcomes_are_idempotent_per_report_and_kind() {
    let (store, tenant, schema) = fresh_tenant().await;
    let signal = sample_signal("idem-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let report = sample_report(&[&signal], ReportKind::Maintenance);
    tenant.insert_report(&report).await.unwrap();

    // First delivery records; the redelivery is a no-op.
    let pr = "https://github.com/chalk/chalk/pull/5";
    assert!(tenant
        .record_outcome(
            report.id,
            OutcomeKind::Merged,
            Some(pr),
            ts(3, 0),
            None,
            None
        )
        .await
        .unwrap());
    assert!(!tenant
        .record_outcome(
            report.id,
            OutcomeKind::Merged,
            Some(pr),
            ts(3, 5),
            None,
            None
        )
        .await
        .unwrap());
    assert_eq!(
        tenant.outcomes_for_report(report.id).await.unwrap().len(),
        1
    );

    // A different kind (revert after merge) still records.
    assert!(tenant
        .record_outcome(report.id, OutcomeKind::Reverted, None, ts(4, 0), None, None)
        .await
        .unwrap());

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn webhook_delivery_ids_dedupe() {
    let (store, tenant, schema) = fresh_tenant().await;
    assert!(tenant
        .record_webhook_delivery("d-123", ts(1, 0))
        .await
        .unwrap());
    assert!(!tenant
        .record_webhook_delivery("d-123", ts(1, 1))
        .await
        .unwrap());
    assert!(tenant
        .record_webhook_delivery("d-456", ts(1, 2))
        .await
        .unwrap());
    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn fetch_cursors_round_trip() {
    let (store, tenant, schema) = fresh_tenant().await;
    assert_eq!(tenant.fetch_cursor("sentry").await.unwrap(), None);
    tenant
        .set_fetch_cursor("sentry", Some("cursor-1"), ts(1, 0))
        .await
        .unwrap();
    assert_eq!(
        tenant.fetch_cursor("sentry").await.unwrap().as_deref(),
        Some("cursor-1")
    );
    tenant
        .set_fetch_cursor("sentry", Some("cursor-2"), ts(2, 0))
        .await
        .unwrap();
    assert_eq!(
        tenant.fetch_cursor("sentry").await.unwrap().as_deref(),
        Some("cursor-2")
    );
    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn migration_steps_apply_forward_from_recorded_version() {
    let (store, tenant, schema) = fresh_tenant().await;
    // Simulate a tenant provisioned before v2: drop the v2 objects and
    // record version 1.
    let pool_probe = tenant.fetch_cursor("x").await;
    assert!(pool_probe.is_ok(), "v2 tables exist after fresh provision");

    let raw = Store::connect(&database_url()).await.unwrap();
    // Downgrade bookkeeping via direct SQL through a scratch tenant handle.
    sqlx_downgrade(&schema).await;

    // Re-opening the tenant must apply v2 again.
    let tenant = raw.tenant(&schema).await.unwrap();
    tenant
        .set_fetch_cursor("posthog", Some("c"), ts(1, 0))
        .await
        .expect("v2 fetch_state restored by forward migration");

    store.drop_tenant(&schema).await.unwrap();
}

/// Direct-SQL downgrade helper for the migration test (test-only).
async fn sqlx_downgrade(schema: &str) {
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url())
        .await
        .unwrap();
    for statement in [
        format!("DROP TABLE IF EXISTS \"{schema}\".fetch_state"),
        format!("DROP TABLE IF EXISTS \"{schema}\".webhook_deliveries"),
        format!("DROP INDEX IF EXISTS \"{schema}\".idx_outcomes_report_kind"),
        format!("UPDATE \"{schema}\".schema_meta SET version = 1"),
    ] {
        sqlx::query(&statement).execute(&pool).await.unwrap();
    }
}

#[tokio::test]
async fn transactional_approve_dispatch_and_rollback() {
    let (store, tenant, schema) = fresh_tenant().await;
    let signal = sample_signal("txn-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();
    let mut report = sample_report(&[&signal], ReportKind::Maintenance);
    report.status = ReportStatus::AwaitingReview;
    tenant.insert_report(&report).await.unwrap();

    let extensions = serde_json::json!({"mcp": ["internal-api"], "skills": ["house-style"]});
    tenant
        .approve_for_dispatch(
            report.id,
            "claude-code",
            Some(&extensions),
            "human",
            ts(3, 0),
        )
        .await
        .unwrap();
    assert_eq!(
        tenant.get_report(report.id).await.unwrap().status,
        ReportStatus::Dispatched
    );
    let dispatch = tenant.dispatch(report.id).await.unwrap().unwrap();
    assert_eq!(dispatch.extensions.unwrap()["mcp"][0], "internal-api");

    // Approving a non-awaiting report fails atomically (no dispatch row).
    let err = tenant
        .approve_for_dispatch(report.id, "claude-code", None, "human", ts(3, 1))
        .await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));

    // The GitHub dispatch call failed → rollback restores the inbox state.
    tenant.rollback_dispatch(report.id).await.unwrap();
    assert_eq!(
        tenant.get_report(report.id).await.unwrap().status,
        ReportStatus::AwaitingReview
    );
    assert!(tenant.dispatch(report.id).await.unwrap().is_none());

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn raw_retention_redacts_old_payloads_only() {
    let (store, tenant, schema) = fresh_tenant().await;
    // ingested_at defaults to now() server-side; a cutoff in the future
    // redacts this signal, a cutoff in the past does not.
    let signal = sample_signal("purge-1", ts(1, 0), ts(2, 0));
    tenant.upsert_signal(&signal).await.unwrap();

    let past_cutoff = ts(1, 0) - Duration::days(365);
    assert_eq!(tenant.purge_raw_older_than(past_cutoff).await.unwrap(), 0);

    let future_cutoff = chrono::Utc::now() + Duration::days(1);
    assert_eq!(tenant.purge_raw_older_than(future_cutoff).await.unwrap(), 1);
    let stored = tenant
        .signal_by_fingerprint(&signal.fingerprint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.raw, serde_json::json!({"purged": true}));
    assert_eq!(stored.title, signal.title, "normalized fields survive");
    // Idempotent: already-purged rows are not counted again.
    assert_eq!(tenant.purge_raw_older_than(future_cutoff).await.unwrap(), 0);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn releases_upsert_and_order() {
    let (store, tenant, schema) = fresh_tenant().await;

    tenant
        .upsert_release("v2.4.0", Some("beef"), ts(5, 0), None)
        .await
        .unwrap();
    tenant
        .upsert_release("v2.3.0", None, ts(1, 0), Some("notes"))
        .await
        .unwrap();
    // Idempotent refresh.
    tenant
        .upsert_release("v2.3.0", Some("cafe"), ts(1, 0), None)
        .await
        .unwrap();

    let releases = tenant.releases().await.unwrap();
    assert_eq!(
        releases,
        vec![
            ("v2.3.0".to_string(), ts(1, 0)),
            ("v2.4.0".to_string(), ts(5, 0))
        ]
    );

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn telemetry_computes_rates_and_phase0_gate() {
    let (store, tenant, schema) = fresh_tenant().await;
    let now = ts(30, 0);

    // 12 dispatched work orders: 10 open PRs (6 merged, 3 closed, 1
    // reverted), 2 discarded runs. 12 approvals + 3 dismissals.
    for i in 0..12 {
        let signal = sample_signal(&format!("tel-{i}"), ts(1, 0), ts(2, 0));
        tenant.upsert_signal(&signal).await.unwrap();
        let report = sample_report(&[&signal], ReportKind::Maintenance);
        tenant.insert_report(&report).await.unwrap();
        tenant.approve_report(report.id, ts(10, 0)).await.unwrap();
        tenant
            .record_dispatch(report.id, "claude-code", ts(10, 1))
            .await
            .unwrap();
        if i < 10 {
            let pr = format!("https://github.com/chalk/chalk/pull/{i}");
            tenant
                .record_pr_opened(report.id, &pr, "merge0/x", ts(10, 2), Some(100_000), None)
                .await
                .unwrap();
            let kind = if i < 6 {
                OutcomeKind::Merged
            } else if i < 9 {
                OutcomeKind::Closed
            } else {
                OutcomeKind::Reverted
            };
            // Merged PRs reviewed in 2h, others in 4h.
            let decided = if kind == OutcomeKind::Merged {
                ts(10, 4)
            } else {
                ts(10, 6)
            };
            tenant
                .record_outcome(report.id, kind, Some(&pr), decided, None, Some(100_000))
                .await
                .unwrap();
        } else {
            tenant
                .record_discard(
                    report.id,
                    "repair budget exhausted",
                    "flaky fixture",
                    ts(10, 3),
                    None,
                )
                .await
                .unwrap();
        }
    }
    for i in 0..3 {
        let signal = sample_signal(&format!("tel-dis-{i}"), ts(1, 0), ts(2, 0));
        tenant.upsert_signal(&signal).await.unwrap();
        let report = sample_report(&[&signal], ReportKind::Maintenance);
        tenant.insert_report(&report).await.unwrap();
        let reason = if i == 0 {
            DismissReason::Duplicate
        } else {
            DismissReason::IntendedBehavior
        };
        tenant
            .dismiss_report(report.id, reason, ts(10, 0))
            .await
            .unwrap();
    }

    let snapshot = tenant.telemetry(30, 3, now).await.unwrap();
    assert_eq!(snapshot.counts.dispatched, 12);
    assert_eq!(snapshot.counts.prs_opened, 10);
    assert_eq!(snapshot.counts.prs_merged, 6);
    assert_eq!(snapshot.counts.prs_closed, 3);
    assert_eq!(snapshot.counts.prs_reverted, 1);
    assert_eq!(snapshot.counts.runs_discarded, 2);
    assert_eq!(snapshot.counts.reports_approved, 12);
    assert_eq!(snapshot.counts.dismissals["intended_behavior"], 2);
    assert_eq!(snapshot.counts.dismissals["duplicate"], 1);
    assert_eq!(snapshot.merge_rate, Some(0.6));
    assert!(
        snapshot.phase0_gate_met,
        "6/10 at ≥10 decided meets the gate"
    );
    assert_eq!(snapshot.runner_yield, Some(10.0 / 12.0));
    assert_eq!(snapshot.gate_precision, Some(12.0 / 15.0));
    // Median across 6×2h and 4×4h reviews = 2h.
    assert_eq!(snapshot.counts.median_time_to_review_secs, Some(2 * 3600));
    assert_eq!(snapshot.tokens_per_merged_pr, Some(100_000.0));

    // Outside the window nothing counts.
    let empty = tenant
        .telemetry(30, 3, now + Duration::days(90))
        .await
        .unwrap();
    assert_eq!(empty.counts.dispatched, 0);
    assert!(!empty.phase0_gate_met);

    store.drop_tenant(&schema).await.unwrap();
}
