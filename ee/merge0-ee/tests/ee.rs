//! Integration tests against a real Postgres (same conventions as
//! `merge0-store/tests/store.rs`: local cluster on 55432 or
//! MERGE0_TEST_DATABASE_URL). The control schema `merge0_control` is shared
//! state, so tests serialize on a process-wide lock and each test starts by
//! wiping the control tables (dropping any leftover tenant schemas first —
//! self-healing across crashed runs).

use chrono::{DateTime, Duration, TimeZone, Utc};
use merge0_ee::{
    compute_priors, invoice, usage, Action, EeError, Pricing, Role, Tenant, TenantManager,
    CONTROL_SCHEMA,
};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, OutcomeKind, Report, ReportKind,
    ReportStatus, Severity, Signal, SignalKind, Source,
};
use merge0_store::TenantStore;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};
use ulid::Ulid;

const ACTOR: &str = "ops@example.com";

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Serialize control-plane tests and start from a clean control schema.
async fn setup() -> (TenantManager, PgPool, MutexGuard<'static, ()>) {
    let guard = lock().lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres must be reachable — see README (Testing)");
    let manager = TenantManager::from_pool(pool.clone())
        .await
        .expect("provision control schema");
    wipe(&manager, &pool).await;
    (manager, pool, guard)
}

/// Drop every registered tenant schema, then empty the control tables
/// (members cascade off tenants).
async fn wipe(manager: &TenantManager, pool: &PgPool) {
    for tenant in manager.list_tenants().await.unwrap() {
        manager
            .store()
            .drop_tenant(&tenant.schema_name)
            .await
            .unwrap();
    }
    for table in ["audit_log", "tenants"] {
        sqlx::query(&format!("DELETE FROM \"{CONTROL_SCHEMA}\".{table}"))
            .execute(pool)
            .await
            .unwrap();
    }
}

fn ts(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
}

fn make_signal(source: Source, severity: Severity, tag: &str) -> Signal {
    Signal {
        id: Ulid::new(),
        source,
        source_ref: tag.to_string(),
        kind: SignalKind::Exception,
        severity,
        title: format!("SecretSignal {tag}"),
        body: "boom".into(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Issue,
            label: "issue".into(),
            url: format!("https://tool.example.com/issues/{tag}/"),
        }],
        fingerprint: fingerprint(source, &["issue", tag]),
        join_keys: JoinKeys::default(),
        affected_count: Some(10),
        delegated: false,
        first_seen: ts(1, 0),
        last_seen: ts(2, 0),
        raw: serde_json::json!({ "id": tag }),
    }
}

/// Seed one report (with member signals from `sources`) plus one outcome,
/// returning every identifying string it introduced — the anonymization
/// test's denylist.
async fn seed_outcome_report(
    store: &TenantStore,
    severity: Severity,
    sources: &[Source],
    outcome: OutcomeKind,
    occurred_at: DateTime<Utc>,
) -> Vec<String> {
    let tag = Ulid::new().to_string().to_lowercase();
    let mut identifiers = Vec::new();
    let mut signals = Vec::new();
    for (i, source) in sources.iter().enumerate() {
        let signal = make_signal(*source, severity, &format!("{tag}-{i}"));
        store.upsert_signal(&signal).await.unwrap();
        identifiers.push(signal.title.clone());
        identifiers.push(signal.fingerprint.clone());
        identifiers.push(signal.evidence[0].url.clone());
        signals.push(signal);
    }
    let report = Report {
        id: Ulid::new(),
        kind: ReportKind::Maintenance,
        title: format!("SecretReport {tag}"),
        summary: format!("SecretSummary {tag}"),
        severity,
        evidence: vec![],
        signal_ids: signals.iter().map(|s| s.id).collect(),
        fingerprints: signals.iter().map(|s| s.fingerprint.clone()).collect(),
        suspect_release: None,
        affected_count: Some(42),
        status: ReportStatus::Completed,
        created_at: ts(5, 0),
    };
    store.insert_report(&report).await.unwrap();
    identifiers.push(report.title.clone());
    identifiers.push(report.id.to_string());
    let pr_url = format!("https://github.example.com/acme/secret-repo/pull/{tag}");
    store
        .record_outcome(report.id, outcome, Some(&pr_url), occurred_at, None, None)
        .await
        .unwrap();
    identifiers.push(pr_url);
    identifiers
}

/// The cross-tenant fixture both priors tests share: two live tenants with
/// different outcome mixes plus one suspended tenant, returning every
/// identifier the seeding introduced.
async fn seed_priors_world(manager: &TenantManager) -> (Vec<String>, DateTime<Utc>) {
    let now = ts(30, 0);
    let mut identifiers = Vec::new();

    let tenant_a = manager
        .create_tenant(&format!("acme-{}", Ulid::new()), "scale", ACTOR, ts(1, 0))
        .await
        .unwrap();
    let tenant_b = manager
        .create_tenant(
            &format!("bloop-{}", Ulid::new()),
            "starter",
            ACTOR,
            ts(1, 0),
        )
        .await
        .unwrap();
    let tenant_c = manager
        .create_tenant(
            &format!("cursed-{}", Ulid::new()),
            "starter",
            ACTOR,
            ts(1, 0),
        )
        .await
        .unwrap();
    for tenant in [&tenant_a, &tenant_b, &tenant_c] {
        identifiers.push(tenant.id.to_string());
        identifiers.push(tenant.name.clone());
        identifiers.push(tenant.schema_name.clone());
    }

    let cross = [Source::Sentry, Source::Posthog];
    let single = [Source::Sentry];

    // Tenant A: 6 high/cross-source attempts, 4 merged.
    let store_a = manager.tenant_store(&tenant_a).await.unwrap();
    for (i, outcome) in [
        OutcomeKind::Merged,
        OutcomeKind::Merged,
        OutcomeKind::Merged,
        OutcomeKind::Merged,
        OutcomeKind::Closed,
        OutcomeKind::Closed,
    ]
    .into_iter()
    .enumerate()
    {
        let when = ts(10, i as u32);
        identifiers
            .extend(seed_outcome_report(&store_a, Severity::High, &cross, outcome, when).await);
    }
    // Noise that must NOT count: a discarded run (never a PR) and a merge
    // outside the window.
    identifiers.extend(
        seed_outcome_report(
            &store_a,
            Severity::High,
            &cross,
            OutcomeKind::Discarded,
            ts(10, 20),
        )
        .await,
    );
    identifiers.extend(
        seed_outcome_report(
            &store_a,
            Severity::High,
            &cross,
            OutcomeKind::Merged,
            now - Duration::days(90),
        )
        .await,
    );

    // Tenant B: 4 high/cross attempts (1 merged) + 3 low/single merges
    // (below the minimum sample).
    let store_b = manager.tenant_store(&tenant_b).await.unwrap();
    for (i, outcome) in [
        OutcomeKind::Merged,
        OutcomeKind::Closed,
        OutcomeKind::Closed,
        OutcomeKind::Reverted,
    ]
    .into_iter()
    .enumerate()
    {
        let when = ts(11, i as u32);
        identifiers
            .extend(seed_outcome_report(&store_b, Severity::High, &cross, outcome, when).await);
    }
    for i in 0..3 {
        identifiers.extend(
            seed_outcome_report(
                &store_b,
                Severity::Low,
                &single,
                OutcomeKind::Merged,
                ts(12, i),
            )
            .await,
        );
    }

    // Tenant C: plenty of critical merges — then suspended, so none of it
    // may surface.
    let store_c = manager.tenant_store(&tenant_c).await.unwrap();
    for i in 0..6 {
        identifiers.extend(
            seed_outcome_report(
                &store_c,
                Severity::Critical,
                &single,
                OutcomeKind::Merged,
                ts(13, i),
            )
            .await,
        );
    }
    manager
        .suspend_tenant(tenant_c.id, ACTOR, ts(14, 0))
        .await
        .unwrap();

    (identifiers, now)
}

async fn must_get(manager: &TenantManager, tenant: &Tenant) -> Tenant {
    manager.get_tenant(tenant.id).await.unwrap()
}

#[tokio::test]
async fn tenant_lifecycle_is_audited_and_suspension_gates_the_store() {
    let (manager, pool, _guard) = setup().await;

    let tenant = manager
        .create_tenant("Acme Widgets", "scale", ACTOR, ts(1, 0))
        .await
        .unwrap();
    assert_eq!(
        tenant.schema_name,
        format!("tenant_{}", tenant.id.to_string().to_lowercase())
    );
    assert!(!tenant.suspended);
    assert_eq!(must_get(&manager, &tenant).await, tenant);
    assert_eq!(manager.list_tenants().await.unwrap(), vec![tenant.clone()]);
    assert!(matches!(
        manager.get_tenant(Ulid::new()).await,
        Err(EeError::TenantNotFound(_))
    ));

    // The live tenant's store works end to end.
    let store = manager.tenant_store(&tenant).await.unwrap();
    let signal = make_signal(Source::Sentry, Severity::High, "lifecycle");
    store.upsert_signal(&signal).await.unwrap();

    // Suspension closes the door with a typed error.
    manager
        .suspend_tenant(tenant.id, ACTOR, ts(2, 0))
        .await
        .unwrap();
    let suspended = must_get(&manager, &tenant).await;
    assert!(suspended.suspended);
    assert!(matches!(
        manager.tenant_store(&suspended).await,
        Err(EeError::TenantSuspended(_))
    ));
    assert!(matches!(
        manager.suspend_tenant(Ulid::new(), ACTOR, ts(2, 0)).await,
        Err(EeError::TenantNotFound(_))
    ));

    // Resume reopens it.
    manager
        .resume_tenant(tenant.id, ACTOR, ts(3, 0))
        .await
        .unwrap();
    let resumed = must_get(&manager, &tenant).await;
    assert!(!resumed.suspended);
    manager.tenant_store(&resumed).await.unwrap();

    // Audit trail: every mutation, newest first.
    let entries = manager
        .entries_for_tenant(&tenant.id.to_string(), 10)
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions,
        vec!["tenant.resumed", "tenant.suspended", "tenant.created"]
    );
    assert_eq!(entries[2].actor, ACTOR);
    assert_eq!(entries[2].subject.as_deref(), Some("Acme Widgets"));
    assert_eq!(entries[2].details.as_ref().unwrap()["plan"], "scale");
    // Limit applies after the newest-first ordering.
    let newest = manager
        .entries_for_tenant(&tenant.id.to_string(), 1)
        .await
        .unwrap();
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].action, "tenant.resumed");

    wipe(&manager, &pool).await;
}

#[tokio::test]
async fn membership_rbac_enforces_the_matrix_and_is_audited() {
    let (manager, pool, _guard) = setup().await;

    let tenant = manager
        .create_tenant("Acme Widgets", "scale", ACTOR, ts(1, 0))
        .await
        .unwrap();
    manager
        .add_member(
            tenant.id,
            "rev@example.com",
            Role::Reviewer,
            ACTOR,
            ts(1, 1),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .member_role(tenant.id, "rev@example.com")
            .await
            .unwrap(),
        Some(Role::Reviewer)
    );
    assert_eq!(
        manager
            .member_role(tenant.id, "ghost@example.com")
            .await
            .unwrap(),
        None
    );

    // A reviewer may work the inbox but not manage members.
    manager
        .require(tenant.id, "rev@example.com", Action::ApproveReport)
        .await
        .unwrap();
    let err = manager
        .require(tenant.id, "rev@example.com", Action::ManageMembers)
        .await
        .unwrap_err();
    assert!(matches!(err, EeError::Forbidden { .. }));
    // Non-members are forbidden, not "not found".
    assert!(matches!(
        manager
            .require(tenant.id, "ghost@example.com", Action::ViewInbox)
            .await,
        Err(EeError::Forbidden { .. })
    ));

    // Re-adding upserts the role.
    manager
        .add_member(tenant.id, "rev@example.com", Role::Admin, ACTOR, ts(1, 2))
        .await
        .unwrap();
    assert_eq!(
        manager
            .member_role(tenant.id, "rev@example.com")
            .await
            .unwrap(),
        Some(Role::Admin)
    );
    manager
        .require(tenant.id, "rev@example.com", Action::ManageMembers)
        .await
        .unwrap();

    // Membership of an unknown tenant is a typed not-found.
    assert!(matches!(
        manager
            .add_member(Ulid::new(), "x@example.com", Role::Viewer, ACTOR, ts(1, 3))
            .await,
        Err(EeError::TenantNotFound(_))
    ));

    let entries = manager
        .entries_for_tenant(&tenant.id.to_string(), 10)
        .await
        .unwrap();
    let added: Vec<_> = entries
        .iter()
        .filter(|e| e.action == "member.added")
        .collect();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0].subject.as_deref(), Some("rev@example.com"));
    assert_eq!(added[0].details.as_ref().unwrap()["role"], "admin");

    wipe(&manager, &pool).await;
}

#[tokio::test]
async fn usage_meters_the_tenant_telemetry_and_prices_both_models() {
    let (manager, pool, _guard) = setup().await;
    let now = ts(30, 0);

    let tenant = manager
        .create_tenant("Acme Widgets", "scale", ACTOR, ts(1, 0))
        .await
        .unwrap();
    let store = manager.tenant_store(&tenant).await.unwrap();

    // 4 dispatches: 3 open PRs that merge (100k tokens each), 1 stalls.
    for i in 0..4u32 {
        let signal = make_signal(Source::Sentry, Severity::High, &format!("use-{i}"));
        store.upsert_signal(&signal).await.unwrap();
        let report = Report {
            id: Ulid::new(),
            kind: ReportKind::Maintenance,
            title: format!("usage {i}"),
            summary: "usage".into(),
            severity: Severity::High,
            evidence: vec![],
            signal_ids: vec![signal.id],
            fingerprints: vec![signal.fingerprint.clone()],
            suspect_release: None,
            affected_count: None,
            status: ReportStatus::Approved,
            created_at: ts(5, 0),
        };
        store.insert_report(&report).await.unwrap();
        store
            .record_dispatch(report.id, "claude-code", ts(10, i))
            .await
            .unwrap();
        if i < 3 {
            let pr = format!("https://github.example.com/acme/repo/pull/{i}");
            store
                .record_pr_opened(report.id, &pr, "merge0/x", ts(10, i), Some(100_000), None)
                .await
                .unwrap();
            store
                .record_outcome(
                    report.id,
                    OutcomeKind::Merged,
                    Some(&pr),
                    ts(11, i),
                    None,
                    Some(100_000),
                )
                .await
                .unwrap();
        }
    }

    let metered = usage(&manager, &tenant, 30, now).await.unwrap();
    assert_eq!(metered.window_days, 30);
    assert_eq!(metered.merged_prs, 3);
    assert_eq!(metered.dispatched, 4);
    assert_eq!(metered.prs_opened, 3);
    assert_eq!(metered.tokens_spent, 300_000);

    // The same usage priced under both Phase 2 experiments.
    let per_pr = Pricing::try_new_per_merged_pr(500).unwrap();
    assert_eq!(invoice(&per_pr, metered.merged_prs).total_cents, 1500);
    let flat = Pricing::try_new_flat_plus_pool(9900, 2, 400).unwrap();
    assert_eq!(invoice(&flat, metered.merged_prs).total_cents, 9900 + 400);

    // Metering a suspended tenant is refused like everything else.
    manager
        .suspend_tenant(tenant.id, ACTOR, ts(15, 0))
        .await
        .unwrap();
    let suspended = must_get(&manager, &tenant).await;
    assert!(matches!(
        usage(&manager, &suspended, 30, now).await,
        Err(EeError::TenantSuspended(_))
    ));

    wipe(&manager, &pool).await;
}

#[tokio::test]
async fn priors_aggregate_across_tenants_and_exclude_suspended() {
    let (manager, pool, _guard) = setup().await;
    let (_identifiers, now) = seed_priors_world(&manager).await;

    let priors = compute_priors(&manager, 30, now).await.unwrap();

    // high/cross_source: A's 6 attempts (4 merged) + B's 4 (1 merged); the
    // discarded run and the out-of-window merge never count.
    let high_cross = priors.buckets.get("high/cross_source").unwrap();
    assert_eq!((high_cross.attempts, high_cross.merged), (10, 5));
    // low/single_source: B's 3 merges — present but under the sample floor.
    let low_single = priors.buckets.get("low/single_source").unwrap();
    assert_eq!((low_single.attempts, low_single.merged), (3, 3));
    // The suspended tenant's critical merges are gone entirely.
    assert!(
        !priors.buckets.keys().any(|k| k.starts_with("critical/")),
        "suspended tenant leaked into priors: {:?}",
        priors.buckets
    );

    assert_eq!(priors.advice(Severity::High, true), Some(0.5));
    assert_eq!(
        priors.advice(Severity::Low, false),
        None,
        "3 attempts is below the minimum sample"
    );
    assert_eq!(
        priors.as_gate_context(),
        "historical priors: high/cross_source merges at 50% (n=10)"
    );

    wipe(&manager, &pool).await;
}

#[tokio::test]
async fn priors_serialization_contains_no_tenant_identifiers() {
    let (manager, pool, _guard) = setup().await;
    let (identifiers, now) = seed_priors_world(&manager).await;

    let priors = compute_priors(&manager, 30, now).await.unwrap();
    let json = serde_json::to_string(&priors).unwrap();

    assert!(
        json.contains("high/cross_source"),
        "sanity: buckets present"
    );
    assert!(!identifiers.is_empty());
    for identifier in &identifiers {
        assert!(
            !json.contains(identifier),
            "anonymization violated: {identifier:?} appears in {json}"
        );
    }

    wipe(&manager, &pool).await;
}
