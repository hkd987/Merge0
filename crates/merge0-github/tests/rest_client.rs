//! Wiremock coverage for [`RestGitHub`]: request shape, response parsing,
//! error mapping, the create-branch flow (including the fixed head-SHA and
//! existence-probe bugs), and the retry policy.

use chrono::TimeZone;
use merge0_github::api::{BranchProtection, GitHubApi, RestGitHub};
use merge0_github::auth::StaticToken;
use merge0_github::{GitHubError, RepoRef};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> RestGitHub<StaticToken> {
    RestGitHub::new(StaticToken("test-token".into())).with_base_url(server.uri())
}

fn repo() -> RepoRef {
    RepoRef::parse("octo/widgets").unwrap()
}

// ---- repository_dispatch ----

#[tokio::test]
async fn repository_dispatch_handles_204_and_sends_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .and(header("authorization", "Bearer test-token"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .repository_dispatch(&repo(), "merge0-work-order", &json!({ "id": "wo-1" }))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["event_type"], "merge0-work-order");
    assert_eq!(body["client_payload"]["id"], "wo-1");
}

// ---- branch_protection ----

#[tokio::test]
async fn branch_protection_present_sets_both_flags() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/branches/main/protection"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "required_status_checks": { "strict": true, "contexts": ["ci"] },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let protection = client(&server)
        .branch_protection(&repo(), "main")
        .await
        .unwrap();
    assert!(protection.protected);
    assert!(protection.required_checks);
}

#[tokio::test]
async fn branch_protection_404_means_unprotected_not_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/branches/main/protection"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({ "message": "Branch not protected" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let protection = client(&server)
        .branch_protection(&repo(), "main")
        .await
        .expect("404 must map to the unprotected default, not an error");
    assert_eq!(protection, BranchProtection::default());
}

// ---- get_file_content ----

#[tokio::test]
async fn get_file_content_decodes_base64_with_embedded_newlines() {
    let server = MockServer::start().await;
    // "hello world" base64-encoded, with the newline GitHub inserts.
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/MERGE0.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "aGVsbG8g\nd29ybGQ=\n",
            "encoding": "base64",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let content = client(&server)
        .get_file_content(&repo(), "MERGE0.md")
        .await
        .unwrap();
    assert_eq!(content.as_deref(), Some("hello world"));
}

#[tokio::test]
async fn get_file_content_404_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/missing.md"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .expect(1)
        .mount(&server)
        .await;

    let content = client(&server)
        .get_file_content(&repo(), "missing.md")
        .await
        .unwrap();
    assert_eq!(content, None);
}

#[tokio::test]
async fn get_file_content_malformed_base64_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/bad.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "!!!not-base64!!!",
            "encoding": "base64",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server)
        .get_file_content(&repo(), "bad.md")
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitHubError::Api { .. }),
        "expected Api error for undecodable content, got {err:?}"
    );
}

// ---- list_issues ----

#[tokio::test]
async fn list_issues_sends_query_params_and_returns_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/issues"))
        .and(query_param("state", "all"))
        .and(query_param("per_page", "100"))
        .and(query_param("since", "2026-08-01T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 1, "title": "first" },
            { "number": 2, "title": "second" },
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let since = chrono::Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let issues = client(&server)
        .list_issues(&repo(), Some(since))
        .await
        .unwrap();
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0]["number"], 1);
    assert_eq!(issues[1]["title"], "second");
}

// ---- create_branch_with_files ----

/// Mount the repo-metadata and git-ref mocks shared by the branch-creation
/// scenarios: default branch `main` at `sha` and a successful ref creation.
async fn mount_branch_base(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "default_branch": "main" })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "object": { "sha": sha } })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/git/refs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "ref": "created" })))
        .mount(server)
        .await;
}

fn put_bodies(requests: &[wiremock::Request]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::PUT)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn create_branch_with_files_new_file_puts_without_sha() {
    let server = MockServer::start().await;
    mount_branch_base(&server, "abc123").await;
    // Existence probe: 404 → the file is new on this branch.
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .and(query_param("ref", "merge0/fix"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "content": {} })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .create_branch_with_files(
            &repo(),
            "merge0/fix",
            &[("README.md".into(), "# fixed".into())],
            "fix: readme",
        )
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    // The ref was created from the fetched head SHA.
    let ref_body: serde_json::Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|r| r.url.path() == "/repos/octo/widgets/git/refs")
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(ref_body["ref"], "refs/heads/merge0/fix");
    assert_eq!(ref_body["sha"], "abc123");
    // A brand-new file must be PUT without a "sha" key.
    let puts = put_bodies(&requests);
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0]["message"], "fix: readme");
    assert_eq!(puts[0]["branch"], "merge0/fix");
    assert!(
        puts[0].get("sha").is_none(),
        "PUT for a new file must not carry a sha: {}",
        puts[0]
    );
}

#[tokio::test]
async fn create_branch_with_files_existing_file_puts_with_sha() {
    let server = MockServer::start().await;
    mount_branch_base(&server, "abc123").await;
    // Existence probe: the file already exists on the branch.
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .and(query_param("ref", "merge0/fix"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": "existing-blob-sha",
            "content": "",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "content": {} })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .create_branch_with_files(
            &repo(),
            "merge0/fix",
            &[("README.md".into(), "# updated".into())],
            "fix: readme",
        )
        .await
        .unwrap();

    let puts = put_bodies(&server.received_requests().await.unwrap());
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0]["sha"], "existing-blob-sha");
}

#[tokio::test]
async fn create_branch_with_files_missing_head_sha_is_typed_error_and_no_ref_created() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "default_branch": "main" })))
        .mount(&server)
        .await;
    // Unexpected ref shape: no object.sha.
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "object": {} })))
        .expect(1)
        .mount(&server)
        .await;
    // The bug this guards against: an empty SHA silently POSTed to /git/refs.
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/git/refs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
        .expect(0)
        .mount(&server)
        .await;

    let err = client(&server)
        .create_branch_with_files(
            &repo(),
            "merge0/fix",
            &[("README.md".into(), "x".into())],
            "msg",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitHubError::Api { .. }),
        "expected typed Api error for missing object.sha, got {err:?}"
    );
}

#[tokio::test]
async fn create_branch_with_files_probe_500_propagates_and_no_put_attempted() {
    let server = MockServer::start().await;
    mount_branch_base(&server, "abc123").await;
    // Existence probe fails with a 500 — NOT the same as 404/absent.
    // retry-after: 0 keeps the (expected) retries instant; after exhausting
    // 4 attempts the error must propagate.
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .and(query_param("ref", "merge0/fix"))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("retry-after", "0")
                .set_body_json(json!({ "message": "boom" })),
        )
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/octo/widgets/contents/README.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(&server)
        .await;

    let err = client(&server)
        .create_branch_with_files(
            &repo(),
            "merge0/fix",
            &[("README.md".into(), "x".into())],
            "msg",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitHubError::Api { status: 500, .. }),
        "a 500 on the existence probe must propagate, got {err:?}"
    );
}

// ---- retry policy ----

#[tokio::test]
async fn transient_500_is_retried_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "flaky" })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .repository_dispatch(&repo(), "evt", &json!({}))
        .await
        .expect("one 500 then 204 must succeed via retry");
}

#[tokio::test]
async fn rate_limit_429_honors_retry_after_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(json!({ "message": "rate limited" })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    client(&server)
        .repository_dispatch(&repo(), "evt", &json!({}))
        .await
        .expect("429 with retry-after then 204 must succeed");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(1),
        "retry-after: 1 must be honored"
    );
}

#[tokio::test]
async fn secondary_rate_limit_403_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("retry-after", "0")
                .set_body_json(json!({ "message": "secondary rate limit" })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .repository_dispatch(&repo(), "evt", &json!({}))
        .await
        .expect("403 secondary rate limit must be retried");
}

#[tokio::test]
async fn non_retryable_401_fails_immediately_with_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/dispatches"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({ "message": "Bad credentials" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server)
        .repository_dispatch(&repo(), "evt", &json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitHubError::Api { status: 401, .. }),
        "got {err:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "401 must not be retried"
    );
}
