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

/// The same verdict, stated confidently. WORK_JSON omits `confidence`, which
/// parses to Low — so with a tracker configured it is routed to a story
/// rather than dispatched. Tests that mean to exercise *dispatch* must say
/// they are confident, exactly as a real gate would.
const WORK_JSON_HIGH: &str = r#"{"decision":"work","summary":"Fix the crash","repro":"open /districts/sync",
    "success_criteria":"regression test passes","constraints":"stay small","confidence":"high"}"#;

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
    rate_limit_per_second: u32,
    registry: Option<merge0_server::RegistryHandle>,
    gate_toml: Option<&'static str>,
    delivery_mode: merge0_server::handlers::actions::DeliveryMode,
    tracker: Option<Arc<dyn merge0_tracker::Tracker>>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        HarnessOptions {
            protected: true,
            model_responses: vec![WORK_JSON],
            hardening: false,
            rate_limit_per_second: 0,
            registry: None,
            gate_toml: None,
            delivery_mode: merge0_server::handlers::actions::DeliveryMode::Pr,
            tracker: None,
        }
    }
}

impl Harness {
    async fn start(mut options: HarnessOptions) -> Harness {
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
            query_template = "*"
            prompt = "p"
            "#,
        )
        .unwrap()];
        let gate = toml::from_str(options.gate_toml.unwrap_or(
            r#"
            prompt = "gate"
            min_severity = "medium"
            max_work_orders_per_run = 5
            "#,
        ))
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
                jira_shared_token: Some("jira-token".into()),
                linear_signing_secret: Some("linear-secret".into()),
                slack_signing_secret: Some("slack-secret".into()),
                posthog_project_base_url: "https://us.posthog.com/project/1".into(),
                zendesk_agent_base_url: "https://chalk.zendesk.example.com/agent".into(),
                datadog_app_base_url: "https://app.datadog.example.com".into(),
                jira_browse_base_url: "https://chalk-example.atlassian.net/browse".into(),
                slack_team_base_url: "https://chalk-example.slack.com".into(),
            }),
            rate_limiter: merge0_server::ratelimit::RateLimiter::from_rate(
                options.rate_limit_per_second,
            )
            .map(Arc::new),
            reopen_factor: 3,
            efficacy_grace_days: 3,
            notify_reports: true,
            notify_pr_ready: true,
            broker: {
                let mut broker = merge0_broker::Broker::new(merge0_broker::FakeMinter);
                broker.add_runner_key("broker-runner-key");
                Some(Arc::new(tokio::sync::Mutex::new(broker)))
            },
            registry: options.registry.take().map(Arc::new),
            delivery_mode: options.delivery_mode,
            tracker: options.tracker.take(),
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
        "/metrics",
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
    // SPA routes serve the same data-free shell (or an actionable 503 when
    // the UI bundle isn't built in this environment).
    for page in ["/", "/inbox", "/dashboard", "/setup"] {
        let res = h
            .client
            .get(format!("{}{page}", h.base))
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        assert!(
            status == 200 || status == 503,
            "GET {page} must serve the shell or an unbuilt-UI 503, got {status}"
        );
        let body = res.text().await.unwrap();
        assert!(
            !body.contains("TypeError: districtId"),
            "shell carries no report data"
        );
        assert!(!body.contains("api-secret"), "shell carries no token");
    }

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

/// Open routes are per-IP rate limited: a burst beyond capacity gets 429,
/// and the bearer-authed product surface is NOT limited.
#[tokio::test]
async fn open_routes_rate_limit_bursts_with_429() {
    let h = Harness::start(HarnessOptions {
        rate_limit_per_second: 10, // burst capacity 30
        ..Default::default()
    })
    .await;

    // Test connections carry no connect info, so all requests share one
    // bucket — deterministic for this assertion.
    let mut statuses = Vec::new();
    for _ in 0..35 {
        let res = h
            .client
            .get(format!("{}/healthz", h.base))
            .send()
            .await
            .unwrap();
        statuses.push(res.status().as_u16());
    }
    assert!(
        statuses.iter().filter(|s| **s == 200).count() >= 30,
        "burst capacity admitted: {statuses:?}"
    );
    assert!(
        statuses.contains(&429),
        "over-burst requests limited: {statuses:?}"
    );

    // The protected surface stays unlimited.
    for _ in 0..35 {
        let res = h.get("/telemetry").await;
        assert_eq!(res.status(), 200);
    }

    h.teardown().await;
}

/// `/metrics` renders the telemetry snapshot as Prometheus text, behind the
/// bearer token.
#[tokio::test]
async fn metrics_scrape_is_prometheus_text_over_real_counts() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;

    let res = h.get("/metrics").await;
    assert_eq!(res.status(), 200);
    assert!(res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let body = res.text().await.unwrap();
    assert!(body.contains("# TYPE merge0_work_orders_dispatched gauge"));
    assert!(
        body.contains("merge0_reports{status=\"awaiting_review\"} 1"),
        "queue gauge reflects the seeded report: {body}"
    );
    assert!(body.contains("merge0_phase0_gate_met 0"));

    // Silence the unused-variable pedantry honestly: the report exists.
    assert!(!report_id.is_empty());
    h.teardown().await;
}

/// The ticket-source webhooks: Jira (shared token), Linear (HMAC
/// signature), and Slack Events (v0 signature + URL-verification
/// handshake) all verify, normalize, and store.
#[tokio::test]
async fn ticket_source_webhooks_verify_and_normalize() {
    let h = Harness::start(HarnessOptions::default()).await;

    // Jira: shared token.
    let jira = serde_json::json!({
        "webhookEvent": "jira:issue_created",
        "issue": {
            "key": "CHK-77",
            "fields": {
                "summary": "Roster import stalls at 200 students",
                "priority": { "name": "High" },
                "status": { "statusCategory": { "key": "indeterminate" } },
                "created": "2026-08-05T10:00:00.000Z",
                "updated": "2026-08-06T11:00:00.000Z"
            }
        }
    });
    let res = h
        .client
        .post(format!("{}/webhooks/jira", h.base))
        .header("x-merge0-webhook-token", "jira-token")
        .json(&jira)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1);

    // Linear: HMAC over the exact body bytes.
    let linear = serde_json::json!({
        "type": "Issue",
        "action": "create",
        "data": {
            "identifier": "ENG-500",
            "title": "Attendance export empty",
            "priority": 1,
            "createdAt": "2026-08-06T10:00:00.000Z",
            "updatedAt": "2026-08-06T10:30:00.000Z",
            "url": "https://linear.example.com/chalk/issue/ENG-500"
        }
    });
    let linear_bytes = serde_json::to_vec(&linear).unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"linear-secret").unwrap();
    mac.update(&linear_bytes);
    let res = h
        .client
        .post(format!("{}/webhooks/linear", h.base))
        .header("linear-signature", hex::encode(mac.finalize().into_bytes()))
        .header("content-type", "application/json")
        .body(linear_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1);

    // Slack Events: the URL-verification handshake echoes the challenge…
    let sign_slack = |body: &[u8], ts: &str| {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"slack-secret").unwrap();
        mac.update(b"v0:");
        mac.update(ts.as_bytes());
        mac.update(b":");
        mac.update(body);
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    };
    let handshake = serde_json::to_vec(&serde_json::json!({
        "type": "url_verification", "challenge": "chalk-challenge-123"
    }))
    .unwrap();
    let res = h
        .client
        .post(format!("{}/webhooks/slack", h.base))
        .header("x-slack-request-timestamp", "1723100000")
        .header("x-slack-signature", sign_slack(&handshake, "1723100000"))
        .header("content-type", "application/json")
        .body(handshake)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["challenge"], "chalk-challenge-123");

    // …and a channel message event lands as a signal.
    let event = serde_json::to_vec(&serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "channel": "C0123456789",
            "ts": "1723100001.000200",
            "text": "Gradebook import failing for classes over 200",
            "user": "U0456"
        }
    }))
    .unwrap();
    let res = h
        .client
        .post(format!("{}/webhooks/slack", h.base))
        .header("x-slack-request-timestamp", "1723100001")
        .header("x-slack-signature", sign_slack(&event, "1723100001"))
        .header("content-type", "application/json")
        .body(event)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["inserted"], 1);

    // Forged credentials are rejected on all three.
    for (path, header_name, value) in [
        ("jira", "x-merge0-webhook-token", "wrong"),
        ("linear", "linear-signature", "deadbeef"),
        ("slack", "x-slack-signature", "v0=deadbeef"),
    ] {
        let res = h
            .client
            .post(format!("{}/webhooks/{path}", h.base))
            .header(header_name, value)
            .header("x-slack-request-timestamp", "1723100002")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "forged {path} webhook must 401");
    }

    h.teardown().await;
}

/// The autonomy dial: OFF by default (the shipped trust posture), and when
/// an operator enables it, only Work Orders at or above the confidence
/// threshold dispatch — with the actor recorded as `auto`.
#[tokio::test]
async fn auto_dispatch_is_off_by_default_and_confidence_gated_when_enabled() {
    // Default config + a high-confidence verdict: stays in the inbox.
    let h = Harness::start(HarnessOptions {
        model_responses: vec![WORK_HIGH_CONFIDENCE_JSON],
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "awaiting_review");
    assert_eq!(detail["work_order"]["confidence"], "high");
    assert!(detail["dispatch"].is_null(), "no dispatch without a human");
    h.teardown().await;

    // Autonomy enabled but the verdict carries NO confidence → Low →
    // fail-conservative: still a human decision.
    let h = Harness::start(HarnessOptions {
        gate_toml: Some(AUTONOMY_GATE_TOML),
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "awaiting_review");
    h.teardown().await;

    // Autonomy enabled + high confidence → dispatched with actor "auto".
    let h = Harness::start(HarnessOptions {
        gate_toml: Some(AUTONOMY_GATE_TOML),
        model_responses: vec![WORK_HIGH_CONFIDENCE_JSON],
        ..HarnessOptions::default()
    })
    .await;
    h.post(
        "/ingest/sentry",
        Some("api-secret"),
        Some(h.sentry_envelope()),
    )
    .await;
    h.post("/triage/run", Some("api-secret"), None).await;
    let reports: serde_json::Value = h
        .get("/reports?status=dispatched")
        .await
        .json()
        .await
        .unwrap();
    let report_id = reports[0]["id"].as_str().expect("auto-dispatched report");
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["dispatch"]["dispatched_by"], "auto");
    let telemetry: serde_json::Value = h.get("/telemetry").await.json().await.unwrap();
    assert_eq!(telemetry["counts"]["auto_dispatched"], 1);
    h.teardown().await;
}

const WORK_HIGH_CONFIDENCE_JSON: &str = r#"{"decision":"work","summary":"Fix the crash",
    "repro":"open /districts/sync","success_criteria":"regression test passes",
    "constraints":"stay small","confidence":"high"}"#;

const AUTONOMY_GATE_TOML: &str = r#"
prompt = "gate"
min_severity = "medium"
max_work_orders_per_run = 5

[autonomy]
auto_dispatch = true
min_confidence = "high"
"#;

/// The hard spend ceiling: once gate spend crosses the cap mid-run, the
/// remaining candidates stay Pending and the run says so loudly.
#[tokio::test]
async fn token_budget_halts_the_gate_and_leaves_overflow_pending() {
    let h = Harness::start(HarnessOptions {
        gate_toml: Some(
            r#"
            prompt = "gate"
            min_severity = "medium"
            max_work_orders_per_run = 5

            [budget]
            max_tokens_per_day = 1
            "#,
        ),
        model_responses: vec![WORK_JSON, WORK_JSON],
        ..HarnessOptions::default()
    })
    .await;

    // Two distinct defects → two clusters → two gate candidates.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    h.post(
        "/ingest/sentry",
        Some("api-secret"),
        Some(serde_json::json!({
            "endpoint": "issues",
            "payload": [
                {
                    "id": "42", "shortId": "CHALK-1",
                    "title": "TypeError: districtId undefined",
                    "permalink": "https://sentry.example.com/organizations/chalk/issues/42/",
                    "level": "error",
                    "metadata": {"type": "TypeError", "value": "districtId undefined"},
                    "userCount": 30, "firstSeen": "2026-08-06T00:00:00Z", "lastSeen": now,
                },
                {
                    "id": "43", "shortId": "CHALK-2",
                    "title": "Panic: report export queue stalled",
                    "permalink": "https://sentry.example.com/organizations/chalk/issues/43/",
                    "level": "error",
                    "metadata": {"type": "Panic", "value": "export queue stalled"},
                    "userCount": 12, "firstSeen": "2026-08-06T00:00:00Z", "lastSeen": now,
                },
            ]
        })),
    )
    .await;
    let run: serde_json::Value = h
        .post("/triage/run", Some("api-secret"), None)
        .await
        .json()
        .await
        .unwrap();
    // First gate call is allowed (nothing spent yet); its 1000 scripted
    // tokens cross the 1-token cap, so the second candidate never gates.
    assert_eq!(run["work_orders"], 1);
    assert_eq!(run["budget_exhausted"], true);
    let pending: serde_json::Value = h.get("/reports?status=pending").await.json().await.unwrap();
    assert_eq!(
        pending.as_array().unwrap().len(),
        1,
        "over-budget candidate stays pending for the next window"
    );
    h.teardown().await;
}

/// Dismissals are not forever: impact growth past the re-open factor pulls
/// a dismissed report back into the inbox — except `intended_behavior`,
/// which stays closed (its recurrence path is the Opportunity classifier).
#[tokio::test]
async fn dismissed_reports_reopen_when_impact_escalates() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;
    h.post(
        &format!("/reports/{report_id}/dismiss"),
        Some("api-secret"),
        Some(serde_json::json!({"reason": "wont_fix"})),
    )
    .await;

    // Same fingerprint, affected count 30 → 95 (>= 3x the snapshot).
    let mut escalated = h.sentry_envelope();
    escalated["payload"][0]["userCount"] = serde_json::json!(95);
    h.post("/ingest/sentry", Some("api-secret"), Some(escalated))
        .await;
    h.post("/triage/run", Some("api-secret"), None).await;

    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "awaiting_review");
    let summary = detail["report"]["summary"].as_str().unwrap();
    assert!(
        summary.contains("REOPENED") && summary.contains("wont_fix"),
        "prior dismissal must be visible: {summary}"
    );

    // intended_behavior stays closed under identical escalation.
    h.post(
        &format!("/reports/{report_id}/dismiss"),
        Some("api-secret"),
        Some(serde_json::json!({"reason": "intended_behavior"})),
    )
    .await;
    let mut tripled = h.sentry_envelope();
    tripled["payload"][0]["userCount"] = serde_json::json!(500);
    h.post("/ingest/sentry", Some("api-secret"), Some(tripled))
        .await;
    h.post("/triage/run", Some("api-secret"), None).await;
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["report"]["status"], "dismissed",
        "intended_behavior dismissals never re-open"
    );
    h.teardown().await;
}

/// The broker's HTTP face: runner key + approved Work Order → single-use,
/// repo-scoped credential; wrong key 401, wrong repo 403, reuse 409.
#[tokio::test]
async fn broker_issues_single_use_work_order_scoped_credentials() {
    let h = Harness::start(HarnessOptions::default()).await;
    let report_id = h.seed_awaiting_report().await;
    h.post(
        &format!("/reports/{report_id}/approve"),
        Some("api-secret"),
        None,
    )
    .await;

    let request = serde_json::json!({
        "work_order_id": report_id,
        "repo": "chalk/chalk",
    });
    let forged = h
        .post(
            "/broker/credentials",
            Some("wrong-key"),
            Some(request.clone()),
        )
        .await;
    assert_eq!(forged.status(), 401);

    let mismatched = h
        .post(
            "/broker/credentials",
            Some("broker-runner-key"),
            Some(serde_json::json!({
                "work_order_id": report_id,
                "repo": "chalk/other-repo",
            })),
        )
        .await;
    assert_eq!(mismatched.status(), 403);

    let granted = h
        .post(
            "/broker/credentials",
            Some("broker-runner-key"),
            Some(request.clone()),
        )
        .await;
    assert_eq!(granted.status(), 200);
    let body: serde_json::Value = granted.json().await.unwrap();
    assert_eq!(body["token"], "fake-token-chalk-chalk");
    assert!(body["expires_at"].is_string());

    let reused = h
        .post(
            "/broker/credentials",
            Some("broker-runner-key"),
            Some(request),
        )
        .await;
    assert_eq!(reused.status(), 409, "grants are single-use");
    h.teardown().await;
}

/// The registry's HTTP face: a signature-verified index lists skills, and
/// install opens the manifest-change PR (never a server-side toggle).
#[tokio::test]
async fn registry_lists_signed_index_and_installs_via_manifest_pr() {
    use merge0_registry::{
        content_hash, sign_index, AcceptanceTelemetry, RegistryIndex, SigningKey, SkillListing,
    };

    // A throwaway on-disk registry: one proven skill, index signed with a
    // fixed test key.
    let dir = std::env::temp_dir().join(format!("merge0-registry-{}", Ulid::new()));
    let skill_dir = dir.join("skills").join("db-migrations");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let files = vec![(
        "SKILL.md".to_string(),
        "# DB migration review checklist\n".to_string(),
    )];
    std::fs::write(skill_dir.join("SKILL.md"), &files[0].1).unwrap();
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let index = RegistryIndex {
        generated_at: chrono::Utc::now(),
        listings: vec![SkillListing {
            name: "db-migrations".into(),
            version: "1.2.0".into(),
            description: "Schema-change review skill".into(),
            content_sha256: content_hash(&files),
            acceptance: Some(AcceptanceTelemetry {
                runs: 12,
                merge_rate: 0.8,
            }),
        }],
    };
    let signed = sign_index(&index, &signing).unwrap();
    std::fs::write(
        dir.join("index.json"),
        serde_json::json!({
            "index_json": signed.index_json,
            "signature_hex": signed.signature_hex,
        })
        .to_string(),
    )
    .unwrap();

    let h = Harness::start(HarnessOptions {
        registry: Some(merge0_server::RegistryHandle {
            dir: dir.clone(),
            verifying_key: signing.verifying_key(),
        }),
        ..HarnessOptions::default()
    })
    .await;

    let unauthenticated = h.client.get(format!("{}/registry/skills", h.base)).send();
    assert_eq!(unauthenticated.await.unwrap().status(), 401);

    let listing: serde_json::Value = h.get("/registry/skills").await.json().await.unwrap();
    assert_eq!(listing["skills"][0]["name"], "db-migrations");

    let installed: serde_json::Value = h
        .post(
            "/registry/skills/db-migrations/install",
            Some("api-secret"),
            Some(serde_json::json!({})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(installed["installed"], "db-migrations");
    assert!(installed["pr_url"]
        .as_str()
        .unwrap()
        .starts_with("https://"));
    {
        let prs = &h.github.state.lock().unwrap().created_prs;
        assert_eq!(prs.len(), 1);
        assert!(prs[0].4.contains("Acceptance telemetry: 12 runs"));
    }

    std::fs::remove_dir_all(&dir).ok();
    h.teardown().await;
}

/// Story-only delivery: the story IS the artifact. No safety dispatch, no
/// runner, no PR — the report is terminally handed off with the story
/// recorded on it.
#[tokio::test]
async fn story_mode_delivers_a_story_and_never_dispatches() {
    let tracker = Arc::new(merge0_tracker::RecordingTracker::new());
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::Story,
        tracker: Some(tracker.clone()),
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["delivered_as"], "story");
    assert_eq!(body["story_key"], "FAKE-1");

    // Exactly one story, carrying the Work Order's evidence and the
    // anti-loop stamp.
    let stories = tracker.stories();
    assert_eq!(stories.len(), 1);
    assert!(stories[0].description.contains("districts/sync"));
    assert!(stories[0]
        .labels
        .contains(&merge0_signal::ORIGIN_LABEL.to_string()));

    // Terminal handoff, and NOT dispatched: no runner was triggered.
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "handed_off");
    assert!(detail["dispatch"].is_null(), "story mode must not dispatch");
    assert_eq!(detail["story_key"], "FAKE-1");
    assert!(detail["story_url"].as_str().unwrap().contains("FAKE-1"));
    assert!(
        h.github.state.lock().unwrap().dispatches.is_empty(),
        "no repository_dispatch in story mode"
    );

    h.teardown().await;
}

/// Accompany mode: the board reflects work the agent is already doing —
/// story AND dispatch, both recorded.
#[tokio::test]
async fn story_and_pr_mode_files_the_story_and_still_dispatches() {
    let tracker = Arc::new(merge0_tracker::RecordingTracker::new());
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::StoryAndPr,
        tracker: Some(tracker.clone()),
        model_responses: vec![WORK_JSON_HIGH],
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["delivered_as"], "story_and_pr");
    assert_eq!(body["dispatched_to"], "chalk/chalk");

    assert_eq!(tracker.stories().len(), 1);
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "dispatched");
    assert_eq!(detail["dispatch"]["status"], "dispatched");
    assert_eq!(detail["story_key"], "FAKE-1");
    h.teardown().await;
}

/// The two modes deliberately disagree about what a tracker outage means.
#[tokio::test]
async fn tracker_failure_blocks_story_only_delivery_but_never_the_pr() {
    // story-only: the story was the whole delivery, so claiming success
    // would be a lie. Fail, and leave the report reviewable.
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::Story,
        tracker: Some(Arc::new(merge0_tracker::RecordingTracker::failing(
            "jira is down",
        ))),
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;
    let response = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await;
    assert_eq!(response.status(), 500);

    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["report"]["status"], "awaiting_review",
        "a failed story delivery must leave the report retryable"
    );
    assert!(detail["story_key"].is_null());
    h.teardown().await;

    // accompany mode: the PR is the artifact; a board outage must not
    // block the fix.
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::StoryAndPr,
        tracker: Some(Arc::new(merge0_tracker::RecordingTracker::failing(
            "jira is down",
        ))),
        model_responses: vec![WORK_JSON_HIGH],
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;
    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["dispatched_to"], "chalk/chalk");
    assert_eq!(body["delivered_as"], "pr", "no story was filed");
    assert!(body["story_key"].is_null());
    h.teardown().await;
}

/// A story already filed for a report is never filed twice, so a retry
/// after a partial failure cannot litter the board with duplicates.
#[tokio::test]
async fn an_existing_story_is_reused_rather_than_duplicated() {
    let tracker = Arc::new(merge0_tracker::RecordingTracker::new());
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::StoryAndPr,
        tracker: Some(tracker.clone()),
        model_responses: vec![WORK_JSON_HIGH],
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    // Simulate the crash window: the story landed, the approval did not.
    let id = report_id.parse::<Ulid>().unwrap();
    h.tenant
        .set_report_story(id, "ENG-7", "https://tracker.example.com/browse/ENG-7")
        .await
        .unwrap();

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["story_key"], "ENG-7", "the existing story is reused");
    assert!(
        tracker.stories().is_empty(),
        "no second story may be filed for the same report"
    );
    h.teardown().await;
}

/// Confidence routing, end to end: a PR-mode install with a tracker turns a
/// Work Order the gate is NOT confident in into a story instead of an
/// autonomous PR.
///
/// This is the product's answer to a borderline gate decision. Before it,
/// uncertainty was resolved by whichever way the model happened to fall on
/// a given run, and the result was a PR either way.
#[tokio::test]
async fn a_low_confidence_work_order_is_routed_to_a_story_instead_of_dispatched() {
    let tracker = Arc::new(merge0_tracker::RecordingTracker::new());
    let h = Harness::start(HarnessOptions {
        // Configured for PRs — routing, not configuration, changes this.
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::Pr,
        tracker: Some(tracker.clone()),
        model_responses: vec![WORK_JSON], // no confidence field → Low
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["delivered_as"], "story");
    assert_eq!(body["confidence"], "low");
    assert_eq!(
        body["routed_by_confidence"], true,
        "a reviewer expecting a PR must be told why they got a story"
    );

    assert_eq!(tracker.stories().len(), 1, "the work is still queued");

    // The reason survives the request: a reviewer opening this report
    // tomorrow can tell "the gate was unsure" from "this install files
    // stories", which are very different facts about the same outcome.
    let detail: serde_json::Value = h
        .get(&format!("/reports/{report_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["report"]["status"], "handed_off");
    let brief = detail["handoff_brief"].as_str().unwrap();
    assert!(
        brief.contains("gate confidence was low"),
        "the routing reason must be persisted, not only returned: {brief}"
    );

    assert!(
        h.github.state.lock().unwrap().dispatches.is_empty(),
        "a low-confidence Work Order must never become an autonomous PR"
    );
    h.teardown().await;
}

/// The other side of the same rule: confidence at or above the floor
/// dispatches exactly as before, so routing costs the confident path
/// nothing.
#[tokio::test]
async fn a_confident_work_order_still_dispatches_with_a_tracker_configured() {
    let tracker = Arc::new(merge0_tracker::RecordingTracker::new());
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::Pr,
        tracker: Some(tracker.clone()),
        model_responses: vec![WORK_JSON_HIGH],
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["delivered_as"], "pr");
    assert_eq!(body["dispatched_to"], "chalk/chalk");
    assert!(
        tracker.stories().is_empty(),
        "PR mode files no story when it dispatches"
    );
    h.teardown().await;
}

/// Without a tracker the floor has nowhere to route to, so it stays inert
/// rather than turning approvals into no-ops. (Startup warns; see main.rs.)
#[tokio::test]
async fn routing_is_inert_when_no_tracker_is_configured() {
    let h = Harness::start(HarnessOptions {
        delivery_mode: merge0_server::handlers::actions::DeliveryMode::Pr,
        tracker: None,
        model_responses: vec![WORK_JSON], // Low confidence
        ..HarnessOptions::default()
    })
    .await;
    let report_id = h.seed_awaiting_report().await;

    let body: serde_json::Value = h
        .post(
            &format!("/reports/{report_id}/approve"),
            Some("api-secret"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["delivered_as"], "pr");
    assert_eq!(body["dispatched_to"], "chalk/chalk");
    h.teardown().await;
}
