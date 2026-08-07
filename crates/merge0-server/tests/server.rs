//! HTTP integration tests: the real router served on an ephemeral port,
//! real Postgres underneath, fakes for GitHub / model / Slack.

use hmac::Mac;
use merge0_github::api::BranchProtection;
use merge0_github::{FakeGitHub, RepoRef};
use merge0_model::ScriptedModel;
use merge0_runner::AgentKind;
use merge0_server::{app, AppState, VendorWebhooks};
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
    #[allow(dead_code)]
    tenant: TenantStore,
    store: Store,
    schema: String,
    github: Arc<FakeGitHub>,
    slack: Arc<RecordingSink>,
    client: reqwest::Client,
}

struct HarnessOptions {
    protected: bool,
    model_responses: Vec<&'static str>,
    hardening: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        HarnessOptions {
            protected: true,
            model_responses: vec![WORK_JSON],
            hardening: false,
        }
    }
}

impl Harness {
    async fn start(options: HarnessOptions) -> Harness {
        let store = Store::connect(&database_url()).await.unwrap();
        let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
        let tenant = store.tenant(&schema).await.unwrap();

        let github = Arc::new(FakeGitHub::new().with_protection(BranchProtection {
            protected: options.protected,
            required_checks: options.protected,
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
            model: Arc::new(ScriptedModel::new(options.model_responses)),
            github: github.clone(),
            slack: Some(slack.clone()),
            scouts: Arc::new(scouts),
            gate: Arc::new(gate),
            repo: RepoRef::parse("chalk/chalk").unwrap(),
            intent_fallback: Arc::new("schools may lack districts".into()),
            agent: AgentKind::ClaudeCode,
            pr_body_template: Arc::new("${MERGE0_SUMMARY}\n${MERGE0_EVIDENCE}".into()),
            callback_url: "http://localhost/runner/callback".into(),
            inbox_url: "http://localhost".into(),
            api_token: Some("api-secret".into()),
            runner_token: Some("runner-secret".into()),
            webhook_secret: Some("hook-secret".into()),
            slack_signing_secret: Some("slack-secret".into()),
            hardening_enabled: options.hardening,
            fetchers: Arc::new(Vec::new()),
            vendor_webhooks: Arc::new(VendorWebhooks {
                sentry_client_secret: Some("sentry-client-secret".into()),
                posthog_shared_token: Some("posthog-token".into()),
                zendesk_signing_secret: Some("zendesk-secret".into()),
                datadog_shared_token: Some("datadog-token".into()),
                posthog_project_base_url: "https://us.posthog.com/project/1".into(),
                zendesk_agent_base_url: "https://chalk.zendesk.example.com/agent".into(),
                datadog_app_base_url: "https://app.datadog.example.com".into(),
            }),
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

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth("api-secret")
            .send()
            .await
            .unwrap()
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

    fn sign_github(body: &str) -> String {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"hook-secret").unwrap();
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn webhook_with_delivery(
        &self,
        event: &str,
        payload: serde_json::Value,
        delivery: &str,
    ) -> reqwest::Response {
        let body = payload.to_string();
        self.client
            .post(format!("{}/webhooks/github", self.base))
            .header("x-github-event", event)
            .header("x-github-delivery", delivery)
            .header("x-hub-signature-256", Self::sign_github(&body))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn webhook(&self, event: &str, payload: serde_json::Value) -> reqwest::Response {
        self.webhook_with_delivery(event, payload, &Ulid::new().to_string())
            .await
    }

    /// Ingest → triage → return the awaiting-review report id.
    async fn seed_awaiting_report(&self) -> String {
        self.post(
            "/ingest/sentry",
            Some("api-secret"),
            Some(self.sentry_envelope()),
        )
        .await;
        self.post("/triage/run", Some("api-secret"), None).await;
        let reports: serde_json::Value = self
            .get("/reports?status=awaiting_review")
            .await
            .json()
            .await
            .unwrap();
        reports[0]["id"].as_str().unwrap().to_string()
    }

    /// Drive a report all the way to a merged PR; returns (report_id, pr_url).
    async fn seed_merged_pr(&self) -> (String, String) {
        let report_id = self.seed_awaiting_report().await;
        self.post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
        let pr_url = "https://github.com/chalk/chalk/pull/7".to_string();
        self.post(
            "/runner/callback",
            Some("runner-secret"),
            Some(serde_json::json!({
                "report_id": report_id,
                "status": "opened",
                "pr_url": pr_url,
                "branch": "merge0/fix",
                "tokens_spent": 120000,
                "files_changed": 2,
                "total_lines_changed": 40,
            })),
        )
        .await;
        let res = self
            .webhook(
                "pull_request",
                serde_json::json!({
                    "action": "closed",
                    "pull_request": {
                        "html_url": pr_url,
                        "merged": true,
                        "merged_at": chrono::Utc::now()
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        "merge_commit_sha": "feedbeefcafe1234",
                        "title": "Fix the crash",
                        "body": "",
                    }
                }),
            )
            .await;
        assert_eq!(res.status(), 200);
        (report_id, pr_url)
    }
}

#[tokio::test]
async fn full_loop_ingest_triage_approve_callback_merge_telemetry() {
    let h = Harness::start(HarnessOptions::default()).await;

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

    let res = h.post("/triage/run", Some("api-secret"), None).await;
    let run: serde_json::Value = res.json().await.unwrap();
    assert_eq!(run["work_orders"], 1);
    assert_eq!(h.slack.recorded().len(), 1);

    let reports: serde_json::Value = h
        .get("/reports?status=awaiting_review")
        .await
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().unwrap().to_string();

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
        // Attribution rode along even without a manifest (defaults).
        assert_eq!(
            state.dispatches[0].2["attribution"]["test_command"],
            "./merge0-test.sh"
        );
    }

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

    let snapshot: serde_json::Value = h
        .get("/telemetry?window_days=30")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["counts"]["prs_merged"], 1);
    assert_eq!(snapshot["merge_rate"], 1.0);
    assert_eq!(snapshot["tokens_per_merged_pr"], 120000.0);

    h.teardown().await;
}

#[tokio::test]
async fn every_data_route_requires_the_bearer_token() {
    // Audit C2: no unauthenticated read surface. The shell page and healthz
    // are the only open GETs, and neither carries data.
    let h = Harness::start(HarnessOptions::default()).await;
    let detail_path = format!("/reports/{}", Ulid::new());
    for path in [
        "/reports",
        "/reports?status=awaiting_review",
        detail_path.as_str(),
        "/telemetry",
        "/safety",
        "/onboarding",
    ] {
        let res = h
            .client
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "GET {path} must 401 without token");
        let res = h
            .client
            .get(format!("{}{path}", h.base))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "GET {path} must 401 with wrong token");
    }
    for path in ["/triage/run", "/slack/digest", "/ingest/sentry"] {
        let res = h.post(path, None, Some(serde_json::json!({}))).await;
        assert_eq!(res.status(), 401, "POST {path} must 401 without token");
    }
    // Open, data-free surfaces stay reachable.
    let res = h
        .client
        .get(format!("{}/healthz", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let shell = h
        .client
        .get(format!("{}/inbox", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        shell.contains("merge0_token"),
        "shell prompts for the token"
    );
    assert!(!shell.contains("TypeError"), "shell carries no report data");

    h.teardown().await;
}

#[tokio::test]
async fn approve_refuses_when_safety_unverified() {
    let h = Harness::start(HarnessOptions {
        protected: false,
        ..Default::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;
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
async fn webhook_redelivery_and_replay_never_double_count() {
    // Audit C4: idempotency at both layers.
    let h = Harness::start(HarnessOptions::default()).await;
    let (_report_id, pr_url) = h.seed_merged_pr().await;

    let payload = serde_json::json!({
        "action": "closed",
        "pull_request": {
            "html_url": pr_url,
            "merged": true,
            "merged_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "merge_commit_sha": "feedbeefcafe1234",
            "title": "Fix the crash",
            "body": "",
        }
    });

    // Replay with a fresh delivery id → the outcome layer blocks it.
    let res = h
        .webhook_with_delivery("pull_request", payload.clone(), "delivery-1")
        .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["actions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a.as_str().unwrap().contains("duplicate")));

    // Same delivery id again → short-circuited before any processing.
    let res = h
        .webhook_with_delivery("pull_request", payload.clone(), "delivery-1")
        .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["duplicate_delivery"], "delivery-1");

    h.webhook("pull_request", payload).await;
    let snapshot: serde_json::Value = h.get("/telemetry").await.json().await.unwrap();
    assert_eq!(
        snapshot["counts"]["prs_merged"], 1,
        "the Phase 0 metric must not inflate on redelivery"
    );

    h.teardown().await;
}

#[tokio::test]
async fn retried_runner_callback_is_a_recorded_noop() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;
    h.post(
        &format!("/reports/{report_id}/approve"),
        Some("api-secret"),
        None,
    )
    .await;

    let callback = serde_json::json!({
        "report_id": report_id,
        "status": "opened",
        "pr_url": "https://github.com/chalk/chalk/pull/9",
        "branch": "merge0/fix",
        "files_changed": 1,
        "total_lines_changed": 5,
    });
    let res = h
        .post(
            "/runner/callback",
            Some("runner-secret"),
            Some(callback.clone()),
        )
        .await;
    assert_eq!(res.status(), 200);
    let slack_after_first = h.slack.recorded().len();

    // Actions re-run retries the callback: no state change, no re-ping.
    let res = h
        .post("/runner/callback", Some("runner-secret"), Some(callback))
        .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["duplicate"], true);
    assert_eq!(h.slack.recorded().len(), slack_after_first);

    h.teardown().await;
}

#[tokio::test]
async fn slack_interaction_buttons_drive_the_same_actions() {
    // Audit M1: the Slack Approve button now has a receiving endpoint.
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;

    let payload = serde_json::json!({
        "type": "block_actions",
        "actions": [{ "action_id": "approve", "value": report_id }],
    })
    .to_string();
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let base_string = format!("v0:{timestamp}:{payload}");
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"slack-secret").unwrap();
    mac.update(base_string.as_bytes());
    let signature = format!("v0={}", hex::encode(mac.finalize().into_bytes()));

    let res = h
        .client
        .post(format!("{}/slack/interactions", h.base))
        .header("x-slack-request-timestamp", &timestamp)
        .header("x-slack-signature", &signature)
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["approved"], report_id);
    assert_eq!(h.github.state.lock().unwrap().dispatches.len(), 1);

    // Forged signature is rejected.
    let res = h
        .client
        .post(format!("{}/slack/interactions", h.base))
        .header("x-slack-request-timestamp", &timestamp)
        .header("x-slack-signature", "v0=deadbeef")
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    h.teardown().await;
}

#[tokio::test]
async fn merged_pr_with_hardening_enabled_proposes_a_prevention_pr() {
    // Audit M2: the §5c trigger. Enabled via flag; the merged webhook spawns
    // the pass, which opens a [hardening] PR through the fake GitHub.
    let h = Harness::start(HarnessOptions {
        hardening: true,
        ..Default::default()
    })
    .await;
    h.seed_merged_pr().await;

    // The pass runs as a background task; poll briefly for its PR.
    let mut hardening_pr = None;
    for _ in 0..50 {
        {
            let state = h.github.state.lock().unwrap();
            if let Some(pr) = state
                .created_prs
                .iter()
                .find(|p| p.3.starts_with("[hardening]"))
            {
                hardening_pr = Some(pr.3.clone());
            }
        }
        if hardening_pr.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let title = hardening_pr.expect("hardening PR proposed after merge");
    assert!(title.starts_with("[hardening]"));

    // The hardening report reached the inbox.
    let reports: serde_json::Value = h
        .get("/reports?status=awaiting_review")
        .await
        .json()
        .await
        .unwrap();
    assert!(reports
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["kind"] == "hardening"));

    h.teardown().await;
}

#[tokio::test]
async fn onboarding_bundle_serves_runnable_workflow_and_templates() {
    // Audit M6 + C3: a real onboarding surface with a workflow whose
    // referenced commands come from the manifest.
    let h = Harness::start(HarnessOptions::default()).await;
    h.github.state.lock().unwrap().files.insert(
        ".merge0/agent.toml".into(),
        "[test]\ncommand = \"cargo test\"\n".into(),
    );

    let bundle: serde_json::Value = h.get("/onboarding").await.json().await.unwrap();
    let workflow = bundle["files"][".github/workflows/merge0.yml"]
        .as_str()
        .unwrap();
    assert!(workflow.contains("cargo test > /tmp/test-output.txt"));
    assert!(
        !workflow.contains("pr-body.sh"),
        "no dangling file references"
    );
    assert!(
        workflow.contains("envsubst"),
        "PR body rendered from template"
    );
    assert!(bundle["files"]["MERGE0.md"]
        .as_str()
        .unwrap()
        .contains("merge0:managed:start"));
    assert!(bundle["files"][".merge0/agent.toml"]
        .as_str()
        .unwrap()
        .contains("[test]"));
    assert!(bundle["checklist"].as_array().unwrap().len() >= 4);
    assert_eq!(bundle["safety"]["satisfied"], true);

    h.teardown().await;
}

#[tokio::test]
async fn intent_doc_is_fetched_from_the_repo_per_run() {
    // Audit C5: MERGE0.md edits take effect on the next triage run without a
    // restart. Malformed manifest also blocks approval with a clear 409.
    let h = Harness::start(HarnessOptions::default()).await;
    h.github.state.lock().unwrap().files.insert(
        "MERGE0.md".into(),
        "# Rules\n\nno districts is fine\n\n<!-- merge0:managed:start -->\n<!-- merge0:managed:end -->\n"
            .into(),
    );
    let report_id = h.seed_awaiting_report().await;

    // A broken manifest fails the approval loudly instead of dispatching
    // with silently-ignored customer config.
    h.github.state.lock().unwrap().files.insert(
        ".merge0/agent.toml".into(),
        "[[mcp]]\nname = \"x\"\ncommand = \"c\"\nauth_env = \"not-a-name\"\n".into(),
    );
    let res = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
    assert_eq!(res.status(), 409);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("agent.toml"));
    // Approval remains retryable after the customer fixes the manifest.
    h.github
        .state
        .lock()
        .unwrap()
        .files
        .remove(".merge0/agent.toml");
    let res = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
    assert_eq!(res.status(), 200);

    h.teardown().await;
}

#[tokio::test]
async fn dismissals_write_outcome_memory_and_digest_reports_state() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;

    let res = h
        .post(
            &format!("/reports/{report_id}/dismiss"),
            Some("api-secret"),
            Some(serde_json::json!({"reason": "intended_behavior"})),
        )
        .await;
    assert_eq!(res.status(), 200);

    let snapshot: serde_json::Value = h.get("/telemetry").await.json().await.unwrap();
    assert_eq!(snapshot["counts"]["dismissals"]["intended_behavior"], 1);

    let res = h.post("/slack/digest", Some("api-secret"), None).await;
    assert_eq!(res.status(), 200);
    assert!(!h.slack.recorded().is_empty());

    h.teardown().await;
}

#[tokio::test]
async fn over_budget_callback_is_coerced_to_discard_with_salvage() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;
    h.post(
        &format!("/reports/{report_id}/approve"),
        Some("api-secret"),
        None,
    )
    .await;

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
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["dispatch"]["status"], "discarded");
    assert!(detail["dispatch"]["discard_reason"]
        .as_str()
        .unwrap()
        .contains("fix larger than expected"));
    assert!(detail["dispatch"]["diagnosis"]
        .as_str()
        .unwrap()
        .contains("scheduler"));

    h.teardown().await;
}

/// Audit C1: native vendor webhooks — each vendor's own auth scheme is
/// verified, then the payload flows through the ordinary adapters.
#[tokio::test]
async fn native_vendor_webhooks_verify_and_normalize() {
    let h = Harness::start(HarnessOptions::default()).await;

    // PostHog: shared-token scheme. Wrong token is rejected before parsing.
    let posthog = serde_json::json!({"issue": {
        "id": "ph-1", "name": "TypeError",
        "description": "districtId undefined",
        "first_seen": "2026-08-06T05:00:00Z",
        "last_seen": "2026-08-06T06:00:00Z",
        "users": 3
    }});
    let res = h
        .client
        .post(format!("{}/webhooks/posthog", h.base))
        .header("x-merge0-webhook-token", "wrong")
        .json(&posthog)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = h
        .client
        .post(format!("{}/webhooks/posthog", h.base))
        .header("x-merge0-webhook-token", "posthog-token")
        .json(&posthog)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1, "posthog issue normalized + stored");

    // Sentry: HMAC-SHA256 of the exact body bytes with the client secret.
    let sentry = serde_json::json!({"data": {"issue": {
        "id": "42", "shortId": "CHALK-1",
        "title": "TypeError: districtId undefined",
        "permalink": "https://sentry.example.com/organizations/chalk/issues/42/",
        "level": "error",
        "metadata": {"type": "TypeError", "value": "districtId undefined"},
        "userCount": 5,
        "firstSeen": "2026-08-06T04:00:00Z",
        "lastSeen": "2026-08-06T06:00:00Z"
    }}});
    let body_bytes = serde_json::to_vec(&sentry).unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"sentry-client-secret").unwrap();
    mac.update(&body_bytes);
    let signature = hex::encode(mac.finalize().into_bytes());
    let res = h
        .client
        .post(format!("{}/webhooks/sentry", h.base))
        .header("sentry-hook-signature", signature)
        .header("content-type", "application/json")
        .body(body_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1, "sentry issue normalized + stored");

    // Unknown vendors don't exist as a surface.
    let res = h
        .client
        .post(format!("{}/webhooks/nope", h.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    h.teardown().await;
}
