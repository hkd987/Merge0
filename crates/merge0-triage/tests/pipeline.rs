//! End-to-end triage runs against real Postgres with a scripted model:
//! ingest → scouts → clustering → gate → persisted decisions.

use chrono::{DateTime, Duration, TimeZone, Utc};
use merge0_model::ScriptedModel;
use merge0_signal::{
    fingerprint, stack_hash, DismissReason, EvidenceKind, EvidenceLink, GateDecision, JoinKeys,
    ReportKind, ReportStatus, Severity, Signal, SignalKind, Source,
};
use merge0_store::Store;
use merge0_triage::config::{GateConfig, ScoutConfig};
use merge0_triage::pipeline::run_triage;
use ulid::Ulid;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 7, 0, 0, 0).unwrap()
}

fn scouts() -> Vec<ScoutConfig> {
    vec![toml::from_str(
        r#"
        name = "everything"
        description = "d"
        schedule = "nightly"
        sources = ["posthog", "sentry", "zendesk"]
        query_template = "*"
        prompt = "new clusters?"
        "#,
    )
    .unwrap()]
}

fn gate_config(max_orders: u32) -> GateConfig {
    toml::from_str(&format!(
        r#"
        prompt = "you are the gate"
        min_severity = "medium"
        max_work_orders_per_run = {max_orders}
        "#
    ))
    .unwrap()
}

fn exception(source: Source, reference: &str, shash: &str) -> Signal {
    Signal {
        id: Ulid::new(),
        source,
        source_ref: reference.into(),
        kind: SignalKind::Exception,
        severity: Severity::High,
        title: format!("TypeError via {}", source.as_str()),
        body: "boom".into(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: format!("{} issue", source.as_str()),
            url: format!("https://{}.example.com/{reference}", source.as_str()),
        }],
        fingerprint: fingerprint(source, &["issue", reference]),
        join_keys: JoinKeys {
            stack_hash: Some(stack_hash(&[shash])),
            release: Some("v2.3.0".into()),
            ..Default::default()
        },
        affected_count: Some(21),
        first_seen: now() - Duration::hours(20),
        last_seen: now() - Duration::hours(1),
        raw: serde_json::Value::Null,
    }
}

fn rage_click(path: &str) -> Signal {
    Signal {
        id: Ulid::new(),
        source: Source::Posthog,
        source_ref: format!("rageclick:{path}"),
        kind: SignalKind::UxFriction,
        severity: Severity::Medium,
        title: format!("Rage clicks on {path}"),
        body: "3 users".into(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Replay,
            label: "replay".into(),
            url: format!("https://posthog.example.com/replay{path}"),
        }],
        fingerprint: fingerprint(Source::Posthog, &["rageclick", path]),
        join_keys: JoinKeys {
            url_path: Some(path.into()),
            ..Default::default()
        },
        affected_count: Some(3),
        first_seen: now() - Duration::hours(10),
        last_seen: now() - Duration::hours(2),
        raw: serde_json::Value::Null,
    }
}

const WORK_JSON: &str = r#"{"decision":"work","summary":"Fix it","repro":"open the page",
    "success_criteria":"regression test passes","constraints":"small fix only"}"#;

#[tokio::test]
async fn cross_source_cluster_gates_into_one_work_order() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    // Same defect from Sentry and PostHog (shared stack hash) + timeline.
    tenant
        .upsert_signal(&exception(Source::Sentry, "s1", "TypeError"))
        .await
        .unwrap();
    tenant
        .upsert_signal(&exception(Source::Posthog, "p1", "TypeError"))
        .await
        .unwrap();
    tenant
        .upsert_release("v2.3.0", None, now() - Duration::days(5), None)
        .await
        .unwrap();

    let model = ScriptedModel::new([WORK_JSON]);
    let run = run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(3),
        "intent: schools may lack districts",
        "chalk/chalk",
        now(),
    )
    .await
    .unwrap();

    // P0-3: exactly one report referencing both signals.
    assert_eq!(run.candidates, 2);
    assert_eq!(run.reports_created, 1);
    assert_eq!(run.work_orders, 1);
    assert_eq!(run.skips, 0);

    let reports = tenant.list_reports(None).await.unwrap();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report.signal_ids.len(), 2);
    assert_eq!(report.status, ReportStatus::AwaitingReview);
    assert_eq!(report.suspect_release.as_deref(), Some("v2.3.0"));

    let order = tenant.work_order(report.id).await.unwrap().unwrap();
    assert_eq!(order.repo, "chalk/chalk");
    assert_eq!(order.success_criteria, "regression test passes");
    // Evidence from both sources rode into the order.
    assert_eq!(order.evidence.len(), 2);

    // Second run: nothing unassigned remains, no new reports, no model calls.
    let run2 = run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(3),
        "",
        "chalk/chalk",
        now(),
    )
    .await
    .unwrap();
    assert_eq!(run2.reports_created, 0);
    assert_eq!(model.requests().len(), 1);

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn opportunity_reports_hand_off_without_gate_or_work_order() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    tenant.upsert_signal(&rage_click("/export")).await.unwrap();

    // Scripted model has NO responses: any gate call would error the run.
    let model = ScriptedModel::new(Vec::<String>::new());
    let run = run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(3),
        "",
        "chalk/chalk",
        now(),
    )
    .await
    .unwrap();

    assert_eq!(run.reports_created, 1);
    assert_eq!(run.opportunities, 1);
    assert_eq!(run.work_orders, 0);
    assert!(
        model.requests().is_empty(),
        "opportunities never hit the gate"
    );

    let reports = tenant.list_reports(None).await.unwrap();
    assert_eq!(reports[0].kind, ReportKind::Opportunity);
    assert_eq!(reports[0].status, ReportStatus::HandedOff);
    let brief = tenant.handoff_brief(reports[0].id).await.unwrap().unwrap();
    assert!(brief.contains("OPPORTUNITY BRIEF"));
    assert!(tenant.work_order(reports[0].id).await.unwrap().is_none());

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn intended_behavior_history_reroutes_recurrences_to_opportunity() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    // Round 1: a support ticket about /sync is gated as maintenance; the
    // reviewer dismisses it as intended behavior.
    let ticket = Signal {
        kind: SignalKind::Ticket,
        source: Source::Zendesk,
        fingerprint: fingerprint(Source::Zendesk, &["ticket", "77"]),
        ..rage_click("/sync") // keeps join_keys.url_path = "/sync"
    };
    tenant.upsert_signal(&ticket).await.unwrap();
    let model = ScriptedModel::new([WORK_JSON]);
    run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(3),
        "",
        "o/r",
        now(),
    )
    .await
    .unwrap();
    let report = &tenant.list_reports(None).await.unwrap()[0];
    assert_eq!(report.kind, ReportKind::Maintenance);
    tenant
        .dismiss_report(report.id, DismissReason::IntendedBehavior, now())
        .await
        .unwrap();

    // Round 2: a NEW ticket (fresh fingerprint — recurrence never reuses
    // ticket ids) about the same location. Users keep colliding with the
    // design → Opportunity, not another Work Order for the reviewer to
    // re-dismiss.
    let recurrence = Signal {
        id: Ulid::new(),
        fingerprint: fingerprint(Source::Zendesk, &["ticket", "78"]),
        last_seen: now() + Duration::hours(1),
        ..ticket
    };
    tenant.upsert_signal(&recurrence).await.unwrap();

    let model2 = ScriptedModel::new([WORK_JSON]);
    let run = run_triage(
        &tenant,
        &model2,
        &scouts(),
        &gate_config(3),
        "",
        "o/r",
        now() + Duration::hours(2),
    )
    .await
    .unwrap();
    assert_eq!(
        run.opportunities, 1,
        "dismissed-as-intended history reroutes"
    );
    assert!(model2.requests().is_empty());

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn work_order_cap_leaves_overflow_pending_and_skips_persist() {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();

    // Three distinct defects; cap of 1; model: 1 work then (unused) skip.
    for (i, shash) in ["A", "B", "C"].iter().enumerate() {
        tenant
            .upsert_signal(&exception(Source::Sentry, &format!("s{i}"), shash))
            .await
            .unwrap();
    }
    let model = ScriptedModel::new([WORK_JSON]);
    let run = run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(1),
        "",
        "o/r",
        now(),
    )
    .await
    .unwrap();
    assert_eq!(run.reports_created, 3);
    assert_eq!(run.work_orders, 1);
    let pending = tenant
        .list_reports(Some(ReportStatus::Pending))
        .await
        .unwrap();
    assert_eq!(pending.len(), 2, "overflow stays pending for the next run");

    // Next run gates the leftovers: one skip (reason persists), one work.
    let model2 = ScriptedModel::new([
        r#"{"decision":"skip","reason":"flaky vendor noise"}"#,
        WORK_JSON,
    ]);
    let run2 = run_triage(
        &tenant,
        &model2,
        &scouts(),
        &gate_config(5),
        "",
        "o/r",
        now(),
    )
    .await
    .unwrap();
    // Candidates are exhausted (all assigned), but pending reports still gate.
    assert_eq!(run2.reports_created, 0);
    assert_eq!(run2.work_orders + run2.skips, 2);

    let skipped = tenant
        .list_reports(Some(ReportStatus::Skipped))
        .await
        .unwrap();
    assert_eq!(skipped.len(), 1);
    match tenant.gate_decision(skipped[0].id).await.unwrap().unwrap() {
        GateDecision::Skip { reason } => assert_eq!(reason, "flaky vendor noise"),
        other => panic!("expected skip, got {other:?}"),
    }

    store.drop_tenant(&schema).await.unwrap();
}
