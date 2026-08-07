//! HTTP integration tests: the real router served on an ephemeral port,
//! real Postgres underneath, fakes for GitHub / model / Slack.

use merge0_github::api::BranchProtection;
use merge0_github::FakeGitHub;
use merge0_model::ScriptedModel;
use merge0_runner::AgentKind;
use merge0_server::{app, AppState};
use merge0_slack::RecordingSink;
use merge0_store::{Store, TenantStore};
use std::sync::Arc;
use ulid::Ulid;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

const WORK_JSON: &str = r#"{"decision":"work","summary":"Fix the crash","repro":"open /districts/sync",
    "success_criteria":"regression test passes","constraints":"stay small"}"#;

struct Harness {
    base: String,
    #[allow(dead_code)] // handy for future tests that assert on the store directly
    tenant: TenantStore,
    store: Store,
    schema: String,
    github: Arc<FakeGitHub>,
    slack: Arc<RecordingSink>,
    client: reqwest::Client,
}

impl Harness {
    async fn start(protected: bool, model_responses: Vec<&str>) -> Harness {
        let store = Store::connect(&database_url()).await.unwrap();
        let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
        let tenant = store.tenant(&schema).await.unwrap();

        let github = Arc::new(FakeGitHub::new().with_protection(BranchProtection {
            protected,
            required_checks: protected,
        }));
        let slack = Arc::new(RecordingSink::new());
        let scouts = vec![toml::from_str(
            r#"
            name = "all"
            description = "d"
            schedule = "nightly"
            sources = ["posthog", "sentry", "zendesk", "webhook"]
            query_template = "q"
            prompt = "p"
            "#,
        )
        .unwrap()];
        let gate = toml::from_str(
            r#"
            prompt = "gate"
            min_severity = "medium"
            max_work_orders_per_run = 5
            "#,
        )
        .unwrap();

        let state = AppState {
            tenant: tenant.clone(),
            model: Arc::new(ScriptedModel::new(model_responses)),
            github: github.clone(),
            slack: Some(slack.clone()),
            scouts: Arc::new(scouts),
            gate: Arc::new(gate),
            repo: "chalk/chalk".into(),
            intent_text: Arc::new("schools may lack districts".into()),
            agent: AgentKind::ClaudeCode,
            callback_url: "http://localhost/runner/callback".into(),
            inbox_url: "http://localhost".into(),
            api_token: Some("api-secret".into()),
            runner_token: Some("runner-secret".into()),
            webhook_secret: Some("hook-secret".into()),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });

        Harness {
            base,
            tenant,
            store,
            schema,
            github,
            slack,
            client: reqwest::Client::new(),
        }
    }

    async fn teardown(self) {
        self.store.drop_tenant(&self.schema).await.unwrap();
    }

    fn sentry_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": "issues",
            "payload": [{
                "id": "42",
                "shortId": "CHALK-1",
                "title": "TypeError: districtId undefined",
                "permalink": "https://sentry.example.com/organizations/chalk/issues/42/",
                "level": "error",
                "metadata": {"type": "TypeError", "value": "districtId undefined"},
                "userCount": 30,
                "firstSeen": "2026-08-06T00:00:00Z",
                "lastSeen": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }]
        })
    }

    async fn post(
        &self,
        path: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> reqwest::Response {
        let mut request = self.client.post(format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    fn sign(body: &str) -> String {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"hook-secret").unwrap();
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn webhook(&self, event: &str, payload: serde_json::Value) -> reqwest::Response {
        let body = payload.to_string();
        self.client
            .post(format!("{}/webhooks/github", self.base))
            .header("x-github-event", event)
            .header("x-hub-signature-256", Self::sign(&body))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn full_loop_ingest_triage_approve_callback_merge_telemetry() {
    let h = Harness::start(true, vec![WORK_JSON]).await;

    // 1. Ingest a Sentry issue.
    let res = h
        .post(
            "/ingest/sentry",
            Some("api-secret"),
            Some(h.sentry_envelope()),
        )
        .await;
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1);

    // 2. Triage → one work order awaiting review; Slack got one message.
    let res = h.post("/triage/run", Some("api-secret"), None).await;
    assert_eq!(res.status(), 200);
    let run: serde_json::Value = res.json().await.unwrap();
    assert_eq!(run["work_orders"], 1);
    assert_eq!(h.slack.recorded().len(), 1);

    // 3. The inbox lists it; the HTML renders it.
    let reports: serde_json::Value = h
        .client
        .get(format!("{}/reports?status=awaiting_review", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().unwrap().to_string();
    let inbox_html = h
        .client
        .get(format!("{}/inbox", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(inbox_html.contains("TypeError"));

    // 4. Approve → safety verified → dispatched into the customer repo.
    let res = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
    assert_eq!(res.status(), 200);
    {
        let state = h.github.state.lock().unwrap();
        assert_eq!(state.dispatches.len(), 1);
        assert_eq!(state.dispatches[0].1, "merge0-work-order");
    }

    // 5. Runner calls back: PR opened. Slack notified again.
    let res = h
        .post(
            "/runner/callback",
            Some("runner-secret"),
            Some(serde_json::json!({
                "report_id": report_id,
                "status": "opened",
                "pr_url": "https://github.com/chalk/chalk/pull/7",
                "branch": "merge0/fix",
                "tokens_spent": 120000,
                "files_changed": 2,
                "total_lines_changed": 40,
            })),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.slack.recorded().len(), 2);

    // 6. GitHub webhook: PR merged → outcome recorded.
    let res = h
        .webhook(
            "pull_request",
            serde_json::json!({
                "action": "closed",
                "pull_request": {
                    "html_url": "https://github.com/chalk/chalk/pull/7",
                    "merged": true,
                    "merged_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "merge_commit_sha": "feedbeefcafe1234",
                    "title": "Fix the crash",
                    "body": "",
                }
            }),
        )
        .await;
    assert_eq!(res.status(), 200);

    // 7. Telemetry shows the full journey.
    let snapshot: serde_json::Value = h
        .client
        .get(format!("{}/telemetry?window_days=30", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["counts"]["dispatched"], 1);
    assert_eq!(snapshot["counts"]["prs_opened"], 1);
    assert_eq!(snapshot["counts"]["prs_merged"], 1);
    assert_eq!(snapshot["merge_rate"], 1.0);
    assert_eq!(snapshot["tokens_per_merged_pr"], 120000.0);

    // 8. A push event reverting the merge records the hard negative.
    let res = h
        .webhook(
            "push",
            serde_json::json!({
                "commits": [{
                    "id": "0000aaaa",
                    "message": "Revert \"Fix the crash\"\n\nThis reverts commit feedbeefcafe1234.",
                    "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                }]
            }),
        )
        .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["actions"][0]
        .as_str()
        .unwrap()
        .contains("revert recorded"));

    let snapshot: serde_json::Value = h
        .client
        .get(format!("{}/telemetry?window_days=30", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["counts"]["prs_reverted"], 1);

    h.teardown().await;
}

#[tokio::test]
async fn approve_refuses_when_safety_unverified() {
    // FakeGitHub reports no branch protection (P0-9).
    let h = Harness::start(false, vec![WORK_JSON]).await;
    h.post(
        "/ingest/sentry",
        Some("api-secret"),
        Some(h.sentry_envelope()),
    )
    .await;
    h.post("/triage/run", Some("api-secret"), None).await;
    let reports: serde_json::Value = h
        .client
        .get(format!("{}/reports?status=awaiting_review", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().unwrap();

    let res = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
    assert_eq!(res.status(), 409);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("safety"));
    assert!(h.github.state.lock().unwrap().dispatches.is_empty());

    h.teardown().await;
}

#[tokio::test]
async fn auth_is_enforced_on_mutating_endpoints() {
    let h = Harness::start(true, vec![]).await;
    for (path, token) in [
        ("/triage/run", None),
        ("/triage/run", Some("wrong")),
        ("/runner/callback", Some("api-secret")), // wrong token class
    ] {
        let res = h.post(path, token, Some(serde_json::json!({}))).await;
        assert_eq!(res.status(), 401, "{path} with {token:?}");
    }
    // Webhook with a bad signature.
    let res = h
        .client
        .post(format!("{}/webhooks/github", h.base))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", "sha256=deadbeef")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    h.teardown().await;
}

#[tokio::test]
async fn over_budget_callback_is_coerced_to_discard_with_salvage() {
    let h = Harness::start(true, vec![WORK_JSON]).await;
    h.post(
        "/ingest/sentry",
        Some("api-secret"),
        Some(h.sentry_envelope()),
    )
    .await;
    h.post("/triage/run", Some("api-secret"), None).await;
    let reports: serde_json::Value = h
        .client
        .get(format!("{}/reports?status=awaiting_review", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().unwrap().to_string();
    h.post(
        &format!("/reports/{report_id}/approve"),
        Some("api-secret"),
        None,
    )
    .await;

    // Runner claims success but blew the diff budget.
    let res = h
        .post(
            "/runner/callback",
            Some("runner-secret"),
            Some(serde_json::json!({
                "report_id": report_id,
                "status": "opened",
                "pr_url": "https://github.com/chalk/chalk/pull/8",
                "branch": "merge0/fix",
                "diagnosis": "the fix spread into the scheduler",
                "files_changed": 12,
                "total_lines_changed": 900,
            })),
        )
        .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["status"], "discarded");

    let detail: serde_json::Value = h
        .client
        .get(format!("{}/reports/{report_id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["dispatch"]["status"], "discarded");
    assert!(detail["dispatch"]["discard_reason"]
        .as_str()
        .unwrap()
        .contains("fix larger than expected"));
    // Failed-run salvage preserved.
    assert!(detail["dispatch"]["diagnosis"]
        .as_str()
        .unwrap()
        .contains("scheduler"));

    h.teardown().await;
}

#[tokio::test]
async fn dismissals_write_outcome_memory_and_digest_reports_state() {
    let h = Harness::start(true, vec![WORK_JSON]).await;
    h.post(
        "/ingest/sentry",
        Some("api-secret"),
        Some(h.sentry_envelope()),
    )
    .await;
    h.post("/triage/run", Some("api-secret"), None).await;
    let reports: serde_json::Value = h
        .client
        .get(format!("{}/reports?status=awaiting_review", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().unwrap().to_string();

    let res = h
        .post(
            &format!("/reports/{report_id}/dismiss"),
            Some("api-secret"),
            Some(serde_json::json!({"reason": "intended_behavior"})),
        )
        .await;
    assert_eq!(res.status(), 200);

    let snapshot: serde_json::Value = h
        .client
        .get(format!("{}/telemetry", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["counts"]["dismissals"]["intended_behavior"], 1);

    // Digest posts to Slack even with nothing pending.
    let res = h.post("/slack/digest", Some("api-secret"), None).await;
    assert_eq!(res.status(), 200);
    assert!(!h.slack.recorded().is_empty());

    h.teardown().await;
}
