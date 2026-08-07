//! Wiremock coverage for [`AnthropicModel`]: request shape, response
//! parsing, API errors, and the single-retry policy.

use merge0_model::{AnthropicModel, Model, ModelError, ModelRequest};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn model(server: &MockServer) -> AnthropicModel {
    AnthropicModel::new("test-key".into(), "claude-haiku-4-5-20251001".into())
        .with_base_url(server.uri())
}

fn request() -> ModelRequest {
    ModelRequest {
        system: "you are the gate".into(),
        prompt: "triage this report".into(),
        max_tokens: 1024,
    }
}

fn messages_body(text: &str, input_tokens: u64, output_tokens: u64) -> serde_json::Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "model": "claude-haiku-4-5-20251001",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    })
}

#[tokio::test]
async fn happy_path_sends_headers_and_payload_and_parses_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_json(json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 1024,
            "system": "you are the gate",
            "messages": [{ "role": "user", "content": "triage this report" }],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(messages_body(
            "{\"decision\":\"work\"}",
            321,
            123,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let response = model(&server).complete(&request()).await.unwrap();
    assert_eq!(response.text, "{\"decision\":\"work\"}");
    assert_eq!(response.tokens_used, 321 + 123);
}

#[tokio::test]
async fn api_error_surfaces_message_as_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "max_tokens required" },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = model(&server).complete(&request()).await.unwrap_err();
    match err {
        ModelError::Transport(message) => {
            assert!(message.contains("max_tokens required"), "got: {message}");
            assert!(message.contains("400"), "got: {message}");
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_content_block_is_bad_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [],
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = model(&server).complete(&request()).await.unwrap_err();
    assert!(
        matches!(err, ModelError::BadResponse(_)),
        "expected BadResponse, got {err:?}"
    );
}

#[tokio::test]
async fn rate_limit_is_retried_once_and_succeeds() {
    let server = MockServer::start().await;
    // First attempt: 429. `up_to_n_times(1)` stops matching afterwards, so
    // the retry falls through to the 200 mock below.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": { "type": "rate_limit_error", "message": "slow down" },
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(messages_body("recovered", 10, 5)))
        .expect(1)
        .mount(&server)
        .await;

    let response = model(&server).complete(&request()).await.unwrap();
    assert_eq!(response.text, "recovered");
    assert_eq!(response.tokens_used, 15);
}

#[tokio::test]
async fn server_errors_exhaust_the_single_retry() {
    let server = MockServer::start().await;
    // Exactly two attempts (initial + one retry), then the error surfaces.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": { "type": "api_error", "message": "overloaded" },
        })))
        .expect(2)
        .mount(&server)
        .await;

    let err = model(&server).complete(&request()).await.unwrap_err();
    match err {
        ModelError::Transport(message) => {
            assert!(message.contains("overloaded"), "got: {message}")
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}
