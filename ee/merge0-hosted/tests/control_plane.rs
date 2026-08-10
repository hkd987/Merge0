//! Control-plane integration tests: real Postgres, real router.

use merge0_ee::TenantManager;
use merge0_hosted::{app, HostedState};
use std::sync::Arc;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

async fn start() -> (String, reqwest::Client, Arc<TenantManager>) {
    let manager = Arc::new(TenantManager::connect(&database_url()).await.unwrap());
    let state = HostedState {
        manager: manager.clone(),
        admin_token: Some("ee-admin".into()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    (base, reqwest::Client::new(), manager)
}

#[tokio::test]
async fn tenant_lifecycle_rbac_usage_and_audit_over_http() {
    let (base, client, manager) = start().await;

    // Operator token is required.
    let res = client
        .post(format!("{base}/ee/tenants"))
        .json(
            &serde_json::json!({"name": "chalk", "plan": "design-partner",
            "admin_email": "founder@chalk.example"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // Create a tenant (actor recorded for audit; admin seat seeded).
    let res = client
        .post(format!("{base}/ee/tenants"))
        .bearer_auth("ee-admin")
        .header("x-merge0-actor", "operator@merge0.example")
        .json(
            &serde_json::json!({"name": "chalk", "plan": "design-partner",
            "admin_email": "founder@chalk.example"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let tenant: serde_json::Value = res.json().await.unwrap();
    let tenant_id = tenant["id"].as_str().unwrap().to_string();

    // RBAC: a non-member actor cannot manage members…
    let res = client
        .post(format!("{base}/ee/tenants/{tenant_id}/members"))
        .bearer_auth("ee-admin")
        .header("x-merge0-actor", "stranger@example.com")
        .json(&serde_json::json!({"email": "eng@chalk.example", "role": "reviewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // …the seeded admin can.
    let res = client
        .post(format!("{base}/ee/tenants/{tenant_id}/members"))
        .bearer_auth("ee-admin")
        .header("x-merge0-actor", "founder@chalk.example")
        .json(&serde_json::json!({"email": "eng@chalk.example", "role": "reviewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Usage + invoice under per-merged-PR pricing (fresh tenant: zero).
    let res = client
        .get(format!(
            "{base}/ee/tenants/{tenant_id}/usage?pricing=per_merged_pr:1000"
        ))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["usage"]["merged_prs"], 0);
    assert_eq!(body["invoice"]["total_cents"], 0);

    // Market-price validation propagates as a 400.
    let res = client
        .get(format!(
            "{base}/ee/tenants/{tenant_id}/usage?pricing=per_merged_pr:1500"
        ))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    // Audit trail shows the lifecycle, newest first.
    let res = client
        .get(format!("{base}/ee/tenants/{tenant_id}/audit"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap();
    let audit: serde_json::Value = res.json().await.unwrap();
    let audit_actions: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(audit_actions.contains(&"tenant.created"));
    assert!(audit_actions.contains(&"member.added"));

    // Priors endpoint answers (empty buckets on a fresh install).
    let res = client
        .get(format!("{base}/ee/priors"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Cleanup: drop the tenant schema + control rows.
    let schema = tenant["schema_name"].as_str().unwrap();
    manager.store().drop_tenant(schema).await.unwrap();
}

/// The deploy-readiness pass: two tenants, full isolation, the runtime
/// bridge, and suspension semantics — over HTTP, exactly as an operator
/// console or reconciling orchestrator would drive it.
#[tokio::test]
async fn two_tenants_are_isolated_and_runtime_reflects_suspension() {
    let (base, client, manager) = start().await;

    let create = |name: &str, admin: &str| {
        let client = client.clone();
        let base = base.clone();
        let body = serde_json::json!({"name": name, "plan": "team", "admin_email": admin});
        async move {
            let res: serde_json::Value = client
                .post(format!("{base}/ee/tenants"))
                .bearer_auth("ee-admin")
                .header("x-merge0-actor", "operator@merge0.example")
                .json(&body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            res
        }
    };
    let acme = create("acme", "admin@acme.example").await;
    let globex = create("globex", "admin@globex.example").await;
    let acme_id = acme["id"].as_str().unwrap();
    let globex_id = globex["id"].as_str().unwrap();
    assert_ne!(
        acme["schema_name"], globex["schema_name"],
        "each tenant owns a distinct schema"
    );

    // Data isolation at the source of truth: seed a signal into acme's
    // schema through the same path the data plane uses, then prove globex's
    // schema is empty. Schema-per-tenant makes this structural; the test
    // makes it observed.
    let acme_tenant = manager.get_tenant(acme_id.parse().unwrap()).await.unwrap();
    let globex_tenant = manager
        .get_tenant(globex_id.parse().unwrap())
        .await
        .unwrap();
    let acme_store = manager.tenant_store(&acme_tenant).await.unwrap();
    let globex_store = manager.tenant_store(&globex_tenant).await.unwrap();
    let signal = merge0_signal::Signal {
        id: ulid::Ulid::generate(),
        source: merge0_signal::Source::Sentry,
        source_ref: "acme-1".into(),
        kind: merge0_signal::SignalKind::Exception,
        severity: merge0_signal::Severity::High,
        title: "acme-only crash".into(),
        body: "belongs to acme".into(),
        evidence: vec![],
        fingerprint: "sentry:acmeisolation".into(),
        join_keys: merge0_signal::JoinKeys::default(),
        affected_count: Some(3),
        delegated: false,
        first_seen: chrono::Utc::now(),
        last_seen: chrono::Utc::now(),
        raw: serde_json::Value::Null,
    };
    acme_store.upsert_signal(&signal).await.unwrap();
    assert_eq!(acme_store.unassigned_signals().await.unwrap().len(), 1);
    assert_eq!(
        globex_store.unassigned_signals().await.unwrap().len(),
        0,
        "a signal in acme's schema must be invisible from globex's"
    );

    // Audit isolation: acme's trail never shows globex's lifecycle.
    let audit: serde_json::Value = client
        .get(format!("{base}/ee/tenants/{acme_id}/audit"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        audit
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["tenant_id"].as_str() == Some(acme_id)),
        "audit rows must be scoped to the requested tenant"
    );

    // The runtime bridge: fixed env from the plane, secret NAMES only from
    // the operator, and a running desired state.
    let runtime: serde_json::Value = client
        .get(format!("{base}/ee/tenants/{acme_id}/runtime"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runtime["desired_state"], "running");
    assert_eq!(runtime["env"]["MERGE0_TENANT"], acme["schema_name"]);
    let required: Vec<&str> = runtime["env"]["required_from_operator"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(required.contains(&"MERGE0_GITHUB_APP_PRIVATE_KEY"));
    // Name-only discipline: the manifest must never carry a secret VALUE.
    let rendered = runtime.to_string();
    assert!(
        !rendered.contains("BEGIN") && !rendered.contains("Bearer "),
        "runtime manifest must reference secrets by env-var name only"
    );

    // Suspend acme: the runtime flips to suspended for the orchestrator,
    // the schema refuses to open, membership freezes — and globex feels
    // nothing.
    let res = client
        .post(format!("{base}/ee/tenants/{acme_id}/suspend"))
        .bearer_auth("ee-admin")
        .header("x-merge0-actor", "operator@merge0.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let runtime: serde_json::Value = client
        .get(format!("{base}/ee/tenants/{acme_id}/runtime"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runtime["desired_state"], "suspended");

    let acme_tenant = manager.get_tenant(acme_id.parse().unwrap()).await.unwrap();
    assert!(manager.tenant_store(&acme_tenant).await.is_err());

    let res = client
        .post(format!("{base}/ee/tenants/{acme_id}/members"))
        .bearer_auth("ee-admin")
        .header("x-merge0-actor", "admin@acme.example")
        .json(&serde_json::json!({"email": "new@acme.example", "role": "viewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        409,
        "membership must not drift under a suspended tenant"
    );

    // Usage stays READABLE while suspended — billing an org you froze is
    // exactly when you need its numbers.
    let res = client
        .get(format!("{base}/ee/tenants/{acme_id}/usage"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // The neighbor is untouched.
    let runtime: serde_json::Value = client
        .get(format!("{base}/ee/tenants/{globex_id}/runtime"))
        .bearer_auth("ee-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runtime["desired_state"], "running");
    assert_eq!(globex_store.unassigned_signals().await.unwrap().len(), 0);

    // Cleanup.
    for t in [&acme, &globex] {
        manager
            .store()
            .drop_tenant(t["schema_name"].as_str().unwrap())
            .await
            .unwrap();
    }
}

/// The auth sweep the core server has, applied here: every control route
/// refuses without the operator token, with garbage bodies, BEFORE any
/// parsing — and healthz stays open.
#[tokio::test]
async fn every_control_route_requires_the_admin_token_before_parsing() {
    let (base, client, _manager) = start().await;
    let garbage = "{ not json";

    for (method, path) in [
        ("POST", "/ee/tenants"),
        ("GET", "/ee/tenants"),
        ("POST", "/ee/tenants/01K0000000000000000000000A/suspend"),
        ("POST", "/ee/tenants/01K0000000000000000000000A/resume"),
        ("POST", "/ee/tenants/01K0000000000000000000000A/members"),
        ("GET", "/ee/tenants/01K0000000000000000000000A/usage"),
        ("GET", "/ee/tenants/01K0000000000000000000000A/runtime"),
        ("GET", "/ee/tenants/01K0000000000000000000000A/audit"),
        ("GET", "/ee/priors"),
    ] {
        let req = match method {
            "POST" => client
                .post(format!("{base}{path}"))
                .header("content-type", "application/json")
                .body(garbage),
            _ => client.get(format!("{base}{path}")),
        };
        let res = req.send().await.unwrap();
        assert_eq!(
            res.status(),
            401,
            "{method} {path} must 401 without the operator token (not 400: auth precedes parsing)"
        );
    }

    let res = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(res.status(), 200);
}
