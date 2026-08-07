//! Wiremock coverage for [`WebhookSink`]: the one real network client in
//! this crate.

use merge0_slack::{SlackError, SlackSink, WebhookSink};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn post_success_round_trips_json_body() {
    let server = MockServer::start().await;
    let message = json!({
        "blocks": [
            { "type": "section", "text": { "type": "mrkdwn", "text": "*Work Order* ready" } },
        ],
        "text": "Work Order ready",
    });
    Mock::given(method("POST"))
        .and(path("/services/T000/B000/example"))
        .and(header("content-type", "application/json"))
        .and(body_json(&message))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let sink = WebhookSink::new(format!("{}/services/T000/B000/example", server.uri()));
    sink.post(&message).await.unwrap();

    // The body Slack received parses back to exactly what was posted.
    let requests = server.received_requests().await.unwrap();
    let received: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(received, message);
}

#[tokio::test]
async fn non_success_status_is_webhook_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/services/T000/B000/example"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server_error"))
        .expect(1)
        .mount(&server)
        .await;

    let sink = WebhookSink::new(format!("{}/services/T000/B000/example", server.uri()));
    let err = sink.post(&json!({ "text": "hi" })).await.unwrap_err();
    assert!(
        matches!(err, SlackError::WebhookStatus(500)),
        "expected WebhookStatus(500), got {err:?}"
    );
}
