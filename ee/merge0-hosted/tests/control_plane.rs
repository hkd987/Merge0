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
