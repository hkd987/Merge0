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
    DatadogConfig, GithubIssuesConfig, PosthogConfig, SentryConfig, ZendeskConfig,
};
use merge0_fetch::pollers::{
    DatadogPoller, GithubIssuesPoller, PosthogPoller, SentryPoller, ZendeskPoller,
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

fn posthog_poller(server: &MockServer, key_env: &str) -> PosthogPoller {
    set_secret(key_env, "phx_test_key");
    PosthogPoller::from_config(&PosthogConfig {
        enabled: true,
        project_id: "1".into(),
        api_key_env: key_env.into(),
        base_url: server.uri(),
        project_base_url: "https://us.posthog.com/project/1".into(),
    })
    .unwrap()
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

    let outcome = run_fetch(&poller, &tenant, now()).await.unwrap();
    assert_eq!(outcome.source, "posthog");
    assert_eq!(outcome.envelopes, 2);
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
