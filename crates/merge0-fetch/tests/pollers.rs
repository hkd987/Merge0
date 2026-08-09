//! Fetch-layer integration tests: each poller against a wiremock vendor and
//! a real Postgres (see README: local cluster on 55432, or set
//! MERGE0_TEST_DATABASE_URL) — the exact untested-network-code gap the
//! production audit flagged. Vendor response shapes are reused verbatim from
//! each adapter's golden fixtures, so the poller→adapter contract tested
//! here is the same one the conformance harness pins.
//!
//! Env-var hygiene: every test that resolves a secret uses a globally
//! unique env var name, so concurrent tests never race on `set_var`.

use chrono::{DateTime, TimeZone, Utc};
use merge0_fetch::config::{
    AsanaConfig, DatadogConfig, GithubIssuesConfig, IntercomConfig, JiraConfig, LinearConfig,
    MixpanelConfig, OpenpanelConfig, PosthogConfig, SentryConfig, SlackChannel,
    SlackChannelsConfig, TrelloConfig, ZendeskConfig,
};
use merge0_fetch::pollers::{
    AsanaPoller, DatadogPoller, GithubIssuesPoller, IntercomPoller, JiraPoller, LinearPoller,
    MixpanelPoller, OpenpanelPoller, PosthogPoller, RedditPoller, SentryPoller,
    SlackChannelsPoller, TrelloPoller, XPoller, ZendeskPoller,
};
use merge0_fetch::{run_all, run_fetch, FetchError, Fetcher};
use merge0_github::{FakeGitHub, GitHubApi};
use merge0_store::{Store, TenantStore};
use std::sync::Arc;
use ulid::Ulid;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

async fn fresh_tenant() -> (Store, TenantStore, String) {
    let store = Store::connect(&database_url())
        .await
        .expect("test Postgres must be reachable — see README (Testing)");
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.expect("provision tenant");
    (store, tenant, schema)
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
}

/// The vendor-response part of an adapter golden fixture (fixtures are full
/// envelopes; pollers receive only the vendor payload from the wire).
fn fixture_payload(fixture: &str) -> serde_json::Value {
    let envelope: serde_json::Value = serde_json::from_str(fixture).unwrap();
    envelope["payload"].clone()
}

fn set_secret(var: &str, value: &str) {
    std::env::set_var(var, value);
}

// ---- PostHog ----

const POSTHOG_ISSUES: &str =
    include_str!("../../merge0-adapter-posthog/tests/fixtures/error_tracking_issues_typical.json");
const POSTHOG_RAGECLICKS: &str =
    include_str!("../../merge0-adapter-posthog/tests/fixtures/rageclick_events.json");
const POSTHOG_DEAD_CLICKS: &str =
    include_str!("../../merge0-adapter-posthog/tests/fixtures/dead_click_events_typical.json");
const POSTHOG_FUNNELS: &str =
    include_str!("../../merge0-adapter-posthog/tests/fixtures/funnels_typical.json");

fn posthog_poller_with_funnels(
    server: &MockServer,
    key_env: &str,
    funnel_insight_ids: Vec<String>,
) -> PosthogPoller {
    set_secret(key_env, "phx_test_key");
    PosthogPoller::from_config(&PosthogConfig {
        enabled: true,
        project_id: "1".into(),
        api_key_env: key_env.into(),
        base_url: server.uri(),
        project_base_url: "https://us.posthog.com/project/1".into(),
        funnel_insight_ids,
    })
    .unwrap()
}

fn posthog_poller(server: &MockServer, key_env: &str) -> PosthogPoller {
    posthog_poller_with_funnels(server, key_env, vec![])
}

#[tokio::test]
async fn posthog_poller_ingests_and_sends_cursor_on_second_run() {
    let server = MockServer::start().await;
    let poller = posthog_poller(&server, "MERGE0_TEST_PH_KEY_INGEST");
    let (store, tenant, schema) = fresh_tenant().await;

    // First run: no cursor, so no `after` param; both endpoints polled with
    // bearer auth.
    Mock::given(method("GET"))
        .and(path("/api/projects/1/error_tracking/issues"))
        .and(header("authorization", "Bearer phx_test_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(POSTHOG_ISSUES)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$rageclick"))
        .and(query_param_is_missing("after"))
        .and(header("authorization", "Bearer phx_test_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(POSTHOG_RAGECLICKS)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$dead_click"))
        .and(query_param_is_missing("after"))
        .and(header("authorization", "Bearer phx_test_key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(outcome.source, "posthog");
    // No funnel ids configured → issues + rageclicks + dead clicks only.
    assert_eq!(outcome.envelopes, 3);
    // 2 error-tracking issues + 2 rage-click path groups (golden fixtures).
    assert_eq!((outcome.inserted, outcome.updated), (4, 0));
    // Signals actually landed, findable by adapter fingerprints.
    let signals = tenant
        .signals_since(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(signals.len(), 4);
    // Cursor = latest rage-click event timestamp in the fixture.
    let cursor = tenant.fetch_cursor("posthog").await.unwrap();
    assert_eq!(cursor.as_deref(), Some("2026-08-05T16:00:00Z"));

    // Second run: the events request must carry `after={cursor}`.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/error_tracking/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(POSTHOG_ISSUES)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$rageclick"))
        .and(query_param("after", "2026-08-05T16:00:00Z"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$dead_click"))
        .and(query_param("after", "2026-08-05T16:00:00Z"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // Same issues re-fetched → updates, no new inserts; empty events round
    // keeps the previous cursor.
    assert_eq!((outcome.inserted, outcome.updated), (0, 2));
    assert_eq!(
        tenant.fetch_cursor("posthog").await.unwrap().as_deref(),
        Some("2026-08-05T16:00:00Z")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn posthog_poller_fetches_funnels_and_dead_clicks() {
    let server = MockServer::start().await;
    let poller = posthog_poller_with_funnels(
        &server,
        "MERGE0_TEST_PH_KEY_FUNNELS",
        vec!["301".into(), "302".into(), "303".into()],
    );
    let (store, tenant, schema) = fresh_tenant().await;

    Mock::given(method("GET"))
        .and(path("/api/projects/1/error_tracking/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(POSTHOG_ISSUES)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$rageclick"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Dead clicks ride the same events endpoint with the $dead_click name.
    Mock::given(method("GET"))
        .and(path("/api/projects/1/events"))
        .and(query_param("event", "$dead_click"))
        .and(query_param_is_missing("after"))
        .and(header("authorization", "Bearer phx_test_key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_payload(POSTHOG_DEAD_CLICKS)),
        )
        .expect(1)
        .mount(&server)
        .await;
    // One insight GET per configured funnel id; the golden fixture's
    // payload entries are what each request returns from the wire.
    let funnels = fixture_payload(POSTHOG_FUNNELS);
    for (index, insight_id) in ["301", "302", "303"].iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/api/projects/1/insights/{insight_id}")))
            .and(header("authorization", "Bearer phx_test_key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(funnels["results"][index].clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(outcome.envelopes, 4);
    // 2 issues + 0 rageclick groups + 2 dead-click path groups + 2 funnel
    // Signals (the healthy funnel in the fixture yields none).
    assert_eq!((outcome.inserted, outcome.updated), (6, 0));
    let signals = tenant
        .signals_since(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap())
        .await
        .unwrap();
    let dead_clicks: Vec<_> = signals
        .iter()
        .filter(|s| s.source_ref.starts_with("dead_click:"))
        .collect();
    assert_eq!(dead_clicks.len(), 2);
    let funnel_signals: Vec<_> = signals
        .iter()
        .filter(|s| s.source_ref.starts_with("funnel:"))
        .collect();
    assert_eq!(funnel_signals.len(), 2);
    // Cursor = latest dead-click event timestamp (later than any rageclick).
    assert_eq!(
        tenant.fetch_cursor("posthog").await.unwrap().as_deref(),
        Some("2026-08-05T17:00:00Z")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

// ---- Sentry ----

const SENTRY_ISSUES: &str =
    include_str!("../../merge0-adapter-sentry/tests/fixtures/issues_typical.json");

fn sentry_poller(server: &MockServer, token_env: &str) -> SentryPoller {
    set_secret(token_env, "sntrys_test_token");
    SentryPoller::from_config(&SentryConfig {
        enabled: true,
        organization: "acme".into(),
        project: "app".into(),
        auth_token_env: token_env.into(),
        base_url: server.uri(),
    })
    .unwrap()
}

#[tokio::test]
async fn sentry_poller_ingests_and_paginates_via_link_header() {
    let server = MockServer::start().await;
    let poller = sentry_poller(&server, "MERGE0_TEST_SENTRY_TOKEN_INGEST");
    let (store, tenant, schema) = fresh_tenant().await;

    let link_with_next = format!(
        "<{u}?&cursor=100:0:1>; rel=\"previous\"; results=\"false\"; cursor=\"100:0:1\", \
         <{u}?&cursor=100:100:0>; rel=\"next\"; results=\"true\"; cursor=\"100:100:0\"",
        u = server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/api/0/projects/acme/app/issues/"))
        .and(query_param("query", "is:unresolved"))
        .and(query_param_is_missing("cursor"))
        .and(header("authorization", "Bearer sntrys_test_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_with_next.as_str())
                .set_body_json(fixture_payload(SENTRY_ISSUES)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 2, 0)
    );
    assert_eq!(
        tenant.fetch_cursor("sentry").await.unwrap().as_deref(),
        Some("100:100:0")
    );

    // Second run sends the cursor; exhausted pagination (results="false")
    // clears it.
    server.reset().await;
    let link_done = format!(
        "<{u}?&cursor=100:200:0>; rel=\"next\"; results=\"false\"; cursor=\"100:200:0\"",
        u = server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/api/0/projects/acme/app/issues/"))
        .and(query_param("query", "is:unresolved"))
        .and(query_param("cursor", "100:100:0"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_done.as_str())
                .set_body_json(serde_json::json!([])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!((outcome.inserted, outcome.updated), (0, 0));
    assert_eq!(tenant.fetch_cursor("sentry").await.unwrap(), None);
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn sentry_response_without_link_header_clears_cursor() {
    let server = MockServer::start().await;
    let poller = sentry_poller(&server, "MERGE0_TEST_SENTRY_TOKEN_NOLINK");

    Mock::given(method("GET"))
        .and(path("/api/0/projects/acme/app/issues/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let batch = poller.fetch(Some("100:100:0"), now()).await.unwrap();
    assert_eq!(batch.next_cursor, None);
}

// ---- Zendesk ----

const ZENDESK_TICKETS: &str =
    include_str!("../../merge0-adapter-zendesk/tests/fixtures/tickets_typical.json");

fn zendesk_poller(server: &MockServer, email_env: &str, token_env: &str) -> ZendeskPoller {
    set_secret(email_env, "agent@example.com");
    set_secret(token_env, "zd_test_token");
    ZendeskPoller::from_config(&ZendeskConfig {
        enabled: true,
        subdomain: "example".into(),
        email_env: email_env.into(),
        api_token_env: token_env.into(),
        base_url: Some(server.uri()),
        agent_base_url: "https://example.zendesk.com/agent".into(),
    })
    .unwrap()
}

#[tokio::test]
async fn zendesk_poller_ingests_and_advances_end_time_cursor() {
    let server = MockServer::start().await;
    let poller = zendesk_poller(
        &server,
        "MERGE0_TEST_ZD_EMAIL_INGEST",
        "MERGE0_TEST_ZD_TOKEN_INGEST",
    );
    let (store, tenant, schema) = fresh_tenant().await;

    // Basic auth: base64("{email}/token:{token}").
    let expected_auth = format!("Basic {}", b64("agent@example.com/token:zd_test_token"));
    let mut body = serde_json::json!({ "count": 3, "end_time": 1754557200 });
    body["tickets"] = fixture_payload(ZENDESK_TICKETS)["tickets"].clone();
    Mock::given(method("GET"))
        .and(path("/api/v2/incremental/tickets.json"))
        .and(query_param("start_time", "0"))
        .and(header("authorization", expected_auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 3, 0)
    );
    assert_eq!(
        tenant.fetch_cursor("zendesk").await.unwrap().as_deref(),
        Some("1754557200")
    );

    // Second run resumes from end_time.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/incremental/tickets.json"))
        .and(query_param("start_time", "1754557200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "tickets": [], "count": 0, "end_time": 1754560800 }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(
        tenant.fetch_cursor("zendesk").await.unwrap().as_deref(),
        Some("1754560800")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

// ---- Datadog ----

const DATADOG_EVENTS: &str =
    include_str!("../../merge0-adapter-datadog/tests/fixtures/events_typical.json");

fn datadog_poller(server: &MockServer, api_env: &str, app_env: &str) -> DatadogPoller {
    set_secret(api_env, "dd_api_test_key");
    set_secret(app_env, "dd_app_test_key");
    DatadogPoller::from_config(&DatadogConfig {
        enabled: true,
        api_key_env: api_env.into(),
        app_key_env: app_env.into(),
        base_url: server.uri(),
        app_base_url: "https://app.datadoghq.com".into(),
    })
    .unwrap()
}

#[tokio::test]
async fn datadog_poller_ingests_with_default_window_then_cursors_from_now() {
    let server = MockServer::start().await;
    let poller = datadog_poller(
        &server,
        "MERGE0_TEST_DD_API_KEY_INGEST",
        "MERGE0_TEST_DD_APP_KEY_INGEST",
    );
    let (store, tenant, schema) = fresh_tenant().await;

    // First run: no cursor → filter[from] = now - 24h.
    Mock::given(method("GET"))
        .and(path("/api/v2/events"))
        .and(query_param("filter[from]", "2026-08-06T12:00:00Z"))
        .and(header("DD-API-KEY", "dd_api_test_key"))
        .and(header("DD-APPLICATION-KEY", "dd_app_test_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(DATADOG_EVENTS)))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 3, 0)
    );
    assert_eq!(
        tenant.fetch_cursor("datadog").await.unwrap().as_deref(),
        Some("2026-08-07T12:00:00Z")
    );

    // Second run: filter[from] = the persisted cursor (previous now).
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/events"))
        .and(query_param("filter[from]", "2026-08-07T12:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .expect(1)
        .mount(&server)
        .await;
    let later = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
    run_fetch(&poller, &tenant, later).await.unwrap();
    assert_eq!(
        tenant.fetch_cursor("datadog").await.unwrap().as_deref(),
        Some("2026-08-07T13:00:00Z")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

// ---- GitHub Issues ----

const GITHUB_ISSUES: &str =
    include_str!("../../merge0-adapter-github-issues/tests/fixtures/issues_typical.json");

/// Records the `since` each `list_issues` call received; other methods are
/// unreachable in these tests.
struct RecordingGitHub {
    issues: Vec<serde_json::Value>,
    since_calls: std::sync::Mutex<Vec<Option<DateTime<Utc>>>>,
}

#[async_trait::async_trait]
impl GitHubApi for RecordingGitHub {
    async fn list_issues(
        &self,
        _repo: &merge0_github::RepoRef,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<serde_json::Value>, merge0_github::GitHubError> {
        self.since_calls.lock().unwrap().push(since);
        Ok(self.issues.clone())
    }

    async fn repository_dispatch(
        &self,
        _: &merge0_github::RepoRef,
        _: &str,
        _: &serde_json::Value,
    ) -> Result<(), merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn default_branch(
        &self,
        _: &merge0_github::RepoRef,
    ) -> Result<String, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn branch_protection(
        &self,
        _: &merge0_github::RepoRef,
        _: &str,
    ) -> Result<merge0_github::api::BranchProtection, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn create_branch_with_files(
        &self,
        _: &merge0_github::RepoRef,
        _: &str,
        _: &[(String, String)],
        _: &str,
    ) -> Result<(), merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn create_pull_request(
        &self,
        _: &merge0_github::RepoRef,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<merge0_github::PrInfo, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn get_pull_request(
        &self,
        _: &merge0_github::RepoRef,
        _: u64,
    ) -> Result<merge0_github::PullState, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn list_releases(
        &self,
        _: &merge0_github::RepoRef,
    ) -> Result<Vec<merge0_github::ReleaseInfo>, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
    async fn get_file_content(
        &self,
        _: &merge0_github::RepoRef,
        _: &str,
    ) -> Result<Option<String>, merge0_github::GitHubError> {
        unreachable!("not used by the issues poller")
    }
}

#[tokio::test]
async fn github_issues_poller_ingests_and_passes_since_on_second_run() {
    let github = Arc::new(RecordingGitHub {
        issues: fixture_payload(GITHUB_ISSUES).as_array().unwrap().clone(),
        since_calls: std::sync::Mutex::new(vec![]),
    });
    let poller = GithubIssuesPoller::from_config(
        &GithubIssuesConfig {
            enabled: true,
            repo: "acme/chalk".into(),
        },
        github.clone(),
    )
    .unwrap();
    let (store, tenant, schema) = fresh_tenant().await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // 3 raw issues in the fixture, one is a PR the adapter skips.
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 2, 0)
    );
    assert_eq!(
        tenant
            .fetch_cursor("github_issues")
            .await
            .unwrap()
            .as_deref(),
        Some("2026-08-07T12:00:00Z")
    );

    let later = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
    run_fetch(&poller, &tenant, later).await.unwrap();
    {
        // Scoped so the guard drops before the await below (clippy
        // await_holding_lock).
        let calls = github.since_calls.lock().unwrap();
        assert_eq!(*calls, vec![None, Some(now())]);
    }
    store.drop_tenant(&schema).await.unwrap();
}

// ---- error paths ----

#[tokio::test]
async fn non_2xx_is_a_typed_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    let poller = sentry_poller(&server, "MERGE0_TEST_SENTRY_TOKEN_401");

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({"detail": "Invalid token"})),
        )
        .mount(&server)
        .await;

    let err = poller.fetch(None, now()).await.unwrap_err();
    match err {
        FetchError::Api { status, message } => {
            assert_eq!(status, 401);
            assert_eq!(message, "Invalid token");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_env_var_is_a_config_error_naming_the_variable() {
    std::env::remove_var("MERGE0_TEST_SENTRY_TOKEN_MISSING");
    let result = SentryPoller::from_config(&SentryConfig {
        enabled: true,
        organization: "acme".into(),
        project: "app".into(),
        auth_token_env: "MERGE0_TEST_SENTRY_TOKEN_MISSING".into(),
        base_url: "https://sentry.io".into(),
    });
    match result.err().expect("construction must fail") {
        FetchError::Config(message) => {
            assert!(
                message.contains("MERGE0_TEST_SENTRY_TOKEN_MISSING"),
                "{message}"
            );
            assert!(!message.contains("sntrys"), "must never echo a value");
        }
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[tokio::test]
async fn vendor_drift_fails_the_run_with_adapter_error() {
    let server = MockServer::start().await;
    let poller = sentry_poller(&server, "MERGE0_TEST_SENTRY_TOKEN_DRIFT");
    let (store, tenant, schema) = fresh_tenant().await;

    // Sentry suddenly answers an object where the adapter expects a bare
    // array → the run must fail loudly, not skip.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"issues": []})))
        .mount(&server)
        .await;

    let err = run_fetch(&poller, &tenant, now()).await.unwrap_err();
    assert!(matches!(err, FetchError::Adapter(_)), "{err:?}");
    // The failed run must not advance the cursor.
    assert_eq!(tenant.fetch_cursor("sentry").await.unwrap(), None);
    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn run_all_continues_past_a_failing_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let failing = sentry_poller(&server, "MERGE0_TEST_SENTRY_TOKEN_RUNALL");

    let github = Arc::new(FakeGitHub::new());
    let succeeding = GithubIssuesPoller::from_config(
        &GithubIssuesConfig {
            enabled: true,
            repo: "acme/chalk".into(),
        },
        github,
    )
    .unwrap();

    let fetchers: Vec<Box<dyn Fetcher>> = vec![Box::new(failing), Box::new(succeeding)];
    let (store, tenant, schema) = fresh_tenant().await;

    let results = run_all(&fetchers, &tenant, now()).await;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "sentry");
    assert!(matches!(
        results[0].1,
        Err(FetchError::Api { status: 500, .. })
    ));
    assert_eq!(results[1].0, "github_issues");
    let outcome = results[1].1.as_ref().unwrap();
    assert_eq!((outcome.envelopes, outcome.inserted), (1, 0)); // fake serves no issues
    store.drop_tenant(&schema).await.unwrap();
}

/// Minimal padded base64 for the one auth-header assertion (mirrors the
/// known-vector-tested encoder in `merge0_fetch::webhooks`).
fn b64(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = input.as_bytes();
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ---- Jira ----

const JIRA_ISSUES: &str =
    include_str!("../../merge0-adapter-jira/tests/fixtures/issues_typical.json");

#[tokio::test]
async fn jira_poller_ingests_and_bounds_jql_by_cursor() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    set_secret("MERGE0_TEST_JIRA_EMAIL", "bot@example.com");
    set_secret("MERGE0_TEST_JIRA_TOKEN", "jira-test-token");
    let poller = JiraPoller::from_config(&JiraConfig {
        enabled: true,
        base_url: server.uri(),
        email_env: "MERGE0_TEST_JIRA_EMAIL".into(),
        api_token_env: "MERGE0_TEST_JIRA_TOKEN".into(),
        jql: "statusCategory != Done ORDER BY updated ASC".into(),
    })
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/rest/api/3/search/jql"))
        .and(query_param(
            "jql",
            "statusCategory != Done ORDER BY updated ASC",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(JIRA_ISSUES)))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0, "typical fixture issues land");
    let cursor = tenant.fetch_cursor("jira").await.unwrap().unwrap();
    assert_eq!(cursor, "2026-08-07 12:00", "JQL-format minute cursor");

    // Second run: the cursor bound is ANDed into the JQL.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/search/jql"))
        .and(query_param(
            "jql",
            "(statusCategory != Done) AND updated >= \"2026-08-07 12:00\" ORDER BY updated ASC",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "issues": [] })))
        .expect(1)
        .mount(&server)
        .await;
    run_fetch(&poller, &tenant, now()).await.unwrap();

    store.drop_tenant(&schema).await.unwrap();
}

// ---- Linear ----

const LINEAR_ISSUES: &str =
    include_str!("../../merge0-adapter-linear/tests/fixtures/issues_typical.json");

fn linear_poller(server: &MockServer, key_env: &str) -> LinearPoller {
    set_secret(key_env, "lin_test_key");
    LinearPoller::from_config(&LinearConfig {
        enabled: true,
        api_key_env: key_env.into(),
        base_url: server.uri(),
    })
    .unwrap()
}

#[tokio::test]
async fn linear_poller_ingests_graphql_nodes() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    let poller = linear_poller(&server, "MERGE0_TEST_LINEAR_KEY_A");

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "lin_test_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": fixture_payload(LINEAR_ISSUES) }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0);
    let cursor = tenant.fetch_cursor("linear").await.unwrap().unwrap();
    assert!(cursor.starts_with("2026-08-07T12:00:00"));

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn linear_graphql_errors_fail_loudly_despite_http_200() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    let poller = linear_poller(&server, "MERGE0_TEST_LINEAR_KEY_B");

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Authentication required" }]
        })))
        .mount(&server)
        .await;

    let err = run_fetch(&poller, &tenant, now()).await.unwrap_err();
    match err {
        FetchError::Api { message, .. } => assert!(message.contains("Authentication"), "{message}"),
        other => panic!("expected Api error, got {other:?}"),
    }
    store.drop_tenant(&schema).await.unwrap();
}

// ---- Slack channels ----

const SLACK_MESSAGES: &str =
    include_str!("../../merge0-adapter-slack/tests/fixtures/messages_typical.json");

fn slack_poller(server: &MockServer, token_env: &str) -> SlackChannelsPoller {
    set_secret(token_env, "xoxb-example-token");
    SlackChannelsPoller::from_config(&SlackChannelsConfig {
        enabled: true,
        bot_token_env: token_env.into(),
        base_url: server.uri(),
        team_base_url: "https://acme-example.slack.com".into(),
        channels: vec![SlackChannel {
            id: "C0123456789".into(),
            name: "bugs".into(),
        }],
    })
    .unwrap()
}

#[tokio::test]
async fn slack_poller_ingests_and_advances_ts_cursor() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    let poller = slack_poller(&server, "MERGE0_TEST_SLACK_BOT_A");

    let mut payload = fixture_payload(SLACK_MESSAGES);
    payload["ok"] = serde_json::json!(true);
    Mock::given(method("GET"))
        .and(path("/conversations.history"))
        .and(query_param("channel", "C0123456789"))
        .and(query_param_is_missing("oldest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0, "non-bot messages land");
    let cursor = tenant
        .fetch_cursor("slack_channels")
        .await
        .unwrap()
        .unwrap();
    assert!(!cursor.is_empty());

    // Second poll passes the cursor as `oldest`.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/conversations.history"))
        .and(query_param("oldest", cursor.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "messages": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    run_fetch(&poller, &tenant, now()).await.unwrap();

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn slack_ok_false_fails_loudly_naming_the_channel() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    let poller = slack_poller(&server, "MERGE0_TEST_SLACK_BOT_B");

    Mock::given(method("GET"))
        .and(path("/conversations.history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false, "error": "not_in_channel"
        })))
        .mount(&server)
        .await;

    let err = run_fetch(&poller, &tenant, now()).await.unwrap_err();
    match err {
        FetchError::Api { message, .. } => {
            assert!(message.contains("not_in_channel"), "{message}");
            assert!(message.contains("bugs"), "{message}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    store.drop_tenant(&schema).await.unwrap();
}

// ---- Asana ----

const ASANA_TASKS: &str =
    include_str!("../../merge0-adapter-asana/tests/fixtures/tasks_typical.json");

#[tokio::test]
async fn asana_poller_ingests_and_sends_modified_since_on_second_run() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    set_secret("MERGE0_TEST_ASANA_PAT", "asana-test-pat");
    let poller = AsanaPoller::from_config(&AsanaConfig {
        enabled: true,
        pat_env: "MERGE0_TEST_ASANA_PAT".into(),
        base_url: server.uri(),
        project_gids: vec!["120000000000001".into()],
    })
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/projects/120000000000001/tasks"))
        .and(query_param_is_missing("modified_since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(ASANA_TASKS)))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0);
    let cursor = tenant.fetch_cursor("asana").await.unwrap().unwrap();

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/projects/120000000000001/tasks"))
        .and(query_param("modified_since", cursor.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .expect(1)
        .mount(&server)
        .await;
    run_fetch(&poller, &tenant, now()).await.unwrap();

    store.drop_tenant(&schema).await.unwrap();
}

// ---- Trello ----

const TRELLO_CARDS: &str =
    include_str!("../../merge0-adapter-trello/tests/fixtures/cards_typical.json");

#[tokio::test]
async fn trello_poller_ingests_open_cards_with_query_auth() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    set_secret("MERGE0_TEST_TRELLO_KEY", "trello-test-key");
    set_secret("MERGE0_TEST_TRELLO_TOKEN", "trello-test-token");
    let poller = TrelloPoller::from_config(&TrelloConfig {
        enabled: true,
        key_env: "MERGE0_TEST_TRELLO_KEY".into(),
        token_env: "MERGE0_TEST_TRELLO_TOKEN".into(),
        base_url: server.uri(),
        board_ids: vec!["abc123def456".into()],
    })
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/boards/abc123def456/cards/open"))
        .and(query_param("key", "trello-test-key"))
        .and(query_param("token", "trello-test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_payload(TRELLO_CARDS)))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0, "open cards land");

    store.drop_tenant(&schema).await.unwrap();
}

// ---- Intercom ----

const INTERCOM_CONVERSATIONS: &str =
    include_str!("../../merge0-adapter-intercom/tests/fixtures/conversations_typical.json");

#[tokio::test]
async fn intercom_poller_ingests_and_cursors_by_unix_seconds() {
    let server = MockServer::start().await;
    let (store, tenant, schema) = fresh_tenant().await;
    set_secret("MERGE0_TEST_INTERCOM_TOKEN", "intercom-test-token");
    let poller = IntercomPoller::from_config(&IntercomConfig {
        enabled: true,
        access_token_env: "MERGE0_TEST_INTERCOM_TOKEN".into(),
        base_url: server.uri(),
        app_base_url: "https://app.intercom-example.com/a/inbox/abc123".into(),
    })
    .unwrap();

    Mock::given(method("POST"))
        .and(path("/conversations/search"))
        .and(header("Intercom-Version", "2.11"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_payload(INTERCOM_CONVERSATIONS)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert!(outcome.inserted > 0);
    let cursor = tenant.fetch_cursor("intercom").await.unwrap().unwrap();
    assert_eq!(cursor, now().timestamp().to_string());

    store.drop_tenant(&schema).await.unwrap();
}

// ---- Mixpanel ----

fn mixpanel_poller(server: &MockServer, user_env: &str, secret_env: &str) -> MixpanelPoller {
    set_secret(user_env, "svc-account.abc123");
    set_secret(secret_env, "mixpanel-test-secret");
    MixpanelPoller::from_config(&MixpanelConfig {
        enabled: true,
        project_id: "318".into(),
        service_account_user_env: user_env.into(),
        service_account_secret_env: secret_env.into(),
        base_url: server.uri(),
        project_base_url: "https://mixpanel.example.com/project/318".into(),
        funnel_ids: vec![301],
        lookback_days: 7,
    })
    .unwrap()
}

/// The grounded funnels Query API response shape (docs.mixpanel.com):
/// meta.dates + per-date steps/analysis.
fn mixpanel_funnel_response() -> serde_json::Value {
    serde_json::json!({
        "meta": { "dates": ["2026-07-31", "2026-08-07"] },
        "data": {
            "2026-08-07": {
                "steps": [
                    { "count": 3200, "goal": "App Open", "event": "App Open",
                      "step_conv_ratio": 1.0, "overall_conv_ratio": 1.0, "avg_time": 2 },
                    { "count": 1400, "goal": "Signup", "event": "Signup",
                      "step_conv_ratio": 0.4375, "overall_conv_ratio": 0.4375, "avg_time": 55 }
                ],
                "analysis": { "completion": 1400, "starting_amount": 3200, "steps": 2, "worst": 1 }
            },
            "2026-07-31": {
                "steps": [],
                "analysis": { "completion": 0, "starting_amount": 0, "steps": 0, "worst": 0 }
            }
        }
    })
}

#[tokio::test]
async fn mixpanel_poller_polls_configured_funnels_with_names_and_window() {
    let server = MockServer::start().await;
    let poller = mixpanel_poller(&server, "MERGE0_TEST_MX_USER_A", "MERGE0_TEST_MX_SECRET_A");
    let (store, tenant, schema) = fresh_tenant().await;

    // Both endpoints are hit once per round, and this test runs two rounds
    // (the second proves snapshot re-reads update rather than duplicate).
    Mock::given(method("GET"))
        .and(path("/api/query/funnels/list"))
        .and(query_param("project_id", "318"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "funnel_id": 301, "name": "Signup funnel" }
        ])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/query/funnels"))
        .and(query_param("project_id", "318"))
        .and(query_param("funnel_id", "301"))
        .and(query_param("from_date", "2026-07-31"))
        .and(query_param("to_date", "2026-08-07"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mixpanel_funnel_response()))
        .expect(2)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // One envelope; the 56% worst-step drop clears the adapter's floor, so
    // exactly one ux_friction signal lands.
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 1, 0)
    );
    // Aggregates carry no cursor.
    assert_eq!(tenant.fetch_cursor("mixpanel").await.unwrap(), None);

    // Re-reading the same window updates rather than duplicates (the
    // fingerprint-deduped upsert absorbing snapshot re-reads).
    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!((outcome.inserted, outcome.updated), (0, 1));
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[test]
fn mixpanel_without_funnels_fails_at_construction_not_silently() {
    set_secret("MERGE0_TEST_MX_USER_B", "svc");
    set_secret("MERGE0_TEST_MX_SECRET_B", "secret");
    let error = MixpanelPoller::from_config(&MixpanelConfig {
        enabled: true,
        project_id: "318".into(),
        service_account_user_env: "MERGE0_TEST_MX_USER_B".into(),
        service_account_secret_env: "MERGE0_TEST_MX_SECRET_B".into(),
        base_url: "https://mixpanel.example.com".into(),
        project_base_url: "https://mixpanel.example.com/project/318".into(),
        funnel_ids: vec![],
        lookback_days: 7,
    })
    .err()
    .expect("an enabled source that can never signal must not construct");
    assert!(error.to_string().contains("funnel_ids"), "{error}");
}

// ---- OpenPanel ----

fn openpanel_poller(server: &MockServer, id_env: &str, secret_env: &str) -> OpenpanelPoller {
    set_secret(id_env, "op-client-id");
    set_secret(secret_env, "op-client-secret");
    OpenpanelPoller::from_config(&OpenpanelConfig {
        enabled: true,
        project_id: "website".into(),
        client_id_env: id_env.into(),
        client_secret_env: secret_env.into(),
        base_url: server.uri(),
        project_base_url: "https://openpanel.example.com/acme/website".into(),
        error_events: vec!["payment_failed".into()],
        lookback_days: 7,
    })
    .unwrap()
}

/// The grounded /export/events response shape (OpenPanel source:
/// export.controller.ts + event.service.ts).
fn openpanel_events_response(created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "meta": { "count": 6, "totalCount": 6, "pages": 1, "current": 1 },
        "data": (0..6).map(|i| serde_json::json!({
            "id": format!("01J0000000000000000000000{i}"),
            "name": "payment_failed",
            "deviceId": format!("d-{i}"),
            "profileId": format!("p-{i}"),
            "projectId": "website",
            "sessionId": format!("s-{i}"),
            "properties": { "message": "card declined" },
            "createdAt": created_at,
            "country": "US", "city": "Denver", "region": "CO",
            "os": "macOS", "osVersion": "14.5",
            "browser": "Chrome", "browserVersion": "126",
            "device": "desktop", "brand": "", "model": "",
            "path": "/checkout", "origin": "https://app.example.com",
            "referrer": "", "referrerName": "", "referrerType": ""
        })).collect::<Vec<_>>()
    })
}

#[tokio::test]
async fn openpanel_poller_ingests_incrementally_with_read_client_headers() {
    let server = MockServer::start().await;
    let poller = openpanel_poller(&server, "MERGE0_TEST_OP_ID_A", "MERGE0_TEST_OP_SECRET_A");
    let (store, tenant, schema) = fresh_tenant().await;

    // First run: no cursor → start = now - lookback_days.
    Mock::given(method("GET"))
        .and(path("/export/events"))
        .and(query_param("project_id", "website"))
        .and(query_param("event", "payment_failed"))
        .and(query_param("start", "2026-07-31T12:00:00+00:00"))
        .and(header("openpanel-client-id", "op-client-id"))
        .and(header("openpanel-client-secret", "op-client-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openpanel_events_response("2026-08-07T09:30:00.000Z")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // Six raw events, one (name, path) group → one exception signal.
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 1, 0)
    );
    // Cursor = the vendor's own latest createdAt string, verbatim.
    assert_eq!(
        tenant.fetch_cursor("openpanel").await.unwrap().as_deref(),
        Some("2026-08-07T09:30:00.000Z")
    );

    // Second run: start = the persisted cursor; an empty page keeps it.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/export/events"))
        .and(query_param("start", "2026-08-07T09:30:00.000Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "meta": { "count": 0, "totalCount": 0, "pages": 1, "current": 1 },
            "data": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let later = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
    run_fetch(&poller, &tenant, later).await.unwrap();
    assert_eq!(
        tenant.fetch_cursor("openpanel").await.unwrap().as_deref(),
        Some("2026-08-07T09:30:00.000Z")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[test]
fn openpanel_without_error_events_fails_at_construction_not_silently() {
    set_secret("MERGE0_TEST_OP_ID_B", "id");
    set_secret("MERGE0_TEST_OP_SECRET_B", "secret");
    let error = OpenpanelPoller::from_config(&OpenpanelConfig {
        enabled: true,
        project_id: "website".into(),
        client_id_env: "MERGE0_TEST_OP_ID_B".into(),
        client_secret_env: "MERGE0_TEST_OP_SECRET_B".into(),
        base_url: "https://openpanel.example.com".into(),
        project_base_url: "https://openpanel.example.com/acme/website".into(),
        error_events: vec![],
        lookback_days: 7,
    })
    .err()
    .expect("an enabled source that can never signal must not construct");
    assert!(error.to_string().contains("error_events"), "{error}");
}

// ---- Reddit ----

#[tokio::test]
async fn reddit_poller_authenticates_then_reads_new_posts_with_before_cursor() {
    let server = MockServer::start().await;
    set_secret("MERGE0_TEST_REDDIT_ID_A", "reddit-app-id");
    set_secret("MERGE0_TEST_REDDIT_SECRET_A", "reddit-app-secret");
    let poller = RedditPoller::from_config(&merge0_fetch::config::RedditConfig {
        enabled: true,
        subreddits: vec!["chalkapp".into(), "edtech".into()],
        client_id_env: "MERGE0_TEST_REDDIT_ID_A".into(),
        client_secret_env: "MERGE0_TEST_REDDIT_SECRET_A".into(),
        user_agent: "merge0-fetch:test (integration)".into(),
        base_url: server.uri(),
        auth_base_url: server.uri(),
        public_base_url: "https://www.reddit.com".into(),
    })
    .unwrap();
    let (store, tenant, schema) = fresh_tenant().await;

    // Client-credentials exchange: Basic auth + the configured UA.
    Mock::given(method("POST"))
        .and(path("/api/v1/access_token"))
        .and(header("user-agent", "merge0-fetch:test (integration)"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "reddit-bearer-token",
            "token_type": "bearer",
            "expires_in": 86400,
            "scope": "*"
        })))
        .expect(2)
        .mount(&server)
        .await;

    // First round: no cursor → plain newest page over the multireddit.
    Mock::given(method("GET"))
        .and(path("/r/chalkapp+edtech/new"))
        .and(query_param("limit", "100"))
        .and(query_param_is_missing("before"))
        .and(header("authorization", "Bearer reddit-bearer-token"))
        .and(header("user-agent", "merge0-fetch:test (integration)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_payload(include_str!(
                "../../merge0-adapter-reddit/tests/fixtures/typical.json"
            ))),
        )
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // The golden listing carries two t3 posts → two ticket signals.
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 2, 0)
    );
    // Cursor = the NEWEST post's fullname, replayed as `before`.
    assert_eq!(
        tenant.fetch_cursor("reddit").await.unwrap().as_deref(),
        Some("t3_1kw3ah")
    );

    // Second round: `before` sent; an empty page keeps the cursor.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "reddit-bearer-token-2"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/r/chalkapp+edtech/new"))
        .and(query_param("before", "t3_1kw3ah"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "Listing",
            "data": { "after": null, "children": [] }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let later = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
    run_fetch(&poller, &tenant, later).await.unwrap();
    assert_eq!(
        tenant.fetch_cursor("reddit").await.unwrap().as_deref(),
        Some("t3_1kw3ah")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[test]
fn reddit_without_subreddits_fails_at_construction_not_silently() {
    set_secret("MERGE0_TEST_REDDIT_ID_B", "id");
    set_secret("MERGE0_TEST_REDDIT_SECRET_B", "secret");
    let error = RedditPoller::from_config(&merge0_fetch::config::RedditConfig {
        enabled: true,
        subreddits: vec![],
        client_id_env: "MERGE0_TEST_REDDIT_ID_B".into(),
        client_secret_env: "MERGE0_TEST_REDDIT_SECRET_B".into(),
        user_agent: "merge0-fetch:test".into(),
        base_url: "https://oauth.reddit.example.com".into(),
        auth_base_url: "https://www.reddit.example.com".into(),
        public_base_url: "https://www.reddit.example.com".into(),
    })
    .err()
    .expect("an enabled source that can never signal must not construct");
    assert!(error.to_string().contains("subreddits"), "{error}");
}

// ---- X (Twitter) ----

#[tokio::test]
async fn x_poller_searches_mentions_pages_and_advances_since_id() {
    let server = MockServer::start().await;
    set_secret("MERGE0_TEST_X_BEARER_A", "x-bearer-token");
    let poller = XPoller::from_config(&merge0_fetch::config::XConfig {
        enabled: true,
        query: "@acmeapp OR #acmeapp".into(),
        bearer_token_env: "MERGE0_TEST_X_BEARER_A".into(),
        base_url: server.uri(),
    })
    .unwrap();
    let (store, tenant, schema) = fresh_tenant().await;

    // Page 1: the golden response (its meta carries a next_token).
    Mock::given(method("GET"))
        .and(path("/2/tweets/search/recent"))
        .and(query_param("query", "@acmeapp OR #acmeapp"))
        .and(query_param("expansions", "author_id"))
        .and(query_param_is_missing("since_id"))
        .and(query_param_is_missing("next_token"))
        .and(header("authorization", "Bearer x-bearer-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_payload(include_str!(
                "../../merge0-adapter-x/tests/fixtures/typical.json"
            ))),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Page 2: the next_token is followed once, then the round ends.
    Mock::given(method("GET"))
        .and(path("/2/tweets/search/recent"))
        .and(query_param("next_token", "b26v89c19zqg8o3fpds8xample"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "meta": { "result_count": 0 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    // Two posts in the golden page → two ticket signals.
    assert_eq!(
        (outcome.envelopes, outcome.inserted, outcome.updated),
        (1, 2, 0)
    );
    // Cursor = the round's newest id, replayed as since_id.
    assert_eq!(
        tenant.fetch_cursor("x").await.unwrap().as_deref(),
        Some("1821094444555566677")
    );

    // Second round: since_id sent; an empty round keeps the cursor.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/2/tweets/search/recent"))
        .and(query_param("since_id", "1821094444555566677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "meta": { "result_count": 0 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let later = Utc.with_ymd_and_hms(2026, 8, 7, 13, 0, 0).unwrap();
    run_fetch(&poller, &tenant, later).await.unwrap();
    assert_eq!(
        tenant.fetch_cursor("x").await.unwrap().as_deref(),
        Some("1821094444555566677")
    );
    server.verify().await;
    store.drop_tenant(&schema).await.unwrap();
}

#[test]
fn x_without_a_query_fails_at_construction_not_silently() {
    set_secret("MERGE0_TEST_X_BEARER_B", "token");
    let error = XPoller::from_config(&merge0_fetch::config::XConfig {
        enabled: true,
        query: "  ".into(),
        bearer_token_env: "MERGE0_TEST_X_BEARER_B".into(),
        base_url: "https://api.x.example.com".into(),
    })
    .err()
    .expect("an enabled source that can never signal must not construct");
    assert!(error.to_string().contains("query"), "{error}");
}
