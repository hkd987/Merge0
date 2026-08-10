//! Wiremock coverage for the GitHub App installation-token exchange.

use chrono::TimeZone;
use merge0_github::auth::AppAuth;
use merge0_github::GitHubError;
use serde_json::json;
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_rsa_pem() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let mut rng = rand_core::OsRng;
    let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn installation_token_exchanges_jwt_for_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        // A signed RS256 JWT: three dot-separated base64url segments.
        .and(header_regex(
            "authorization",
            r"^Bearer [A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$",
        ))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_test",
            "expires_at": "2026-08-07T13:00:00Z",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let auth = AppAuth::new("777".into(), test_rsa_pem());
    let now = chrono::Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
    let token = auth
        .installation_token(&reqwest::Client::new(), &server.uri(), 42, now)
        .await
        .unwrap();
    assert_eq!(token, "ghs_test");
}

#[tokio::test]
async fn installation_token_401_is_typed_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "message": "A JSON web token could not be decoded",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let auth = AppAuth::new("777".into(), test_rsa_pem());
    let err = auth
        .installation_token(
            &reqwest::Client::new(),
            &server.uri(),
            42,
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    match err {
        GitHubError::Api { status, message } => {
            assert_eq!(status, 401);
            assert!(message.contains("could not be decoded"), "got: {message}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}
