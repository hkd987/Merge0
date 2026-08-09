//! One poller per source (PRD §1). Each poller owns the vendor-specific
//! I/O — URLs, auth headers, cursoring — and emits the adapter envelopes;
//! the paired adapter stays pure. All HTTP pollers use reqwest with a 30s
//! request timeout; the GitHub Issues poller rides the shared
//! [`merge0_github::GitHubApi`] trait instead of raw HTTP.

use crate::FetchError;
use std::time::Duration;

mod asana;
mod datadog;
mod github_issues;
mod intercom;
mod jira;
mod linear;
mod mixpanel;
mod openpanel;
mod posthog;
mod reddit;
mod sentry;
mod slack_channels;
mod trello;
mod x;
mod zendesk;

pub use asana::AsanaPoller;
pub use datadog::DatadogPoller;
pub use github_issues::GithubIssuesPoller;
pub use intercom::IntercomPoller;
pub use jira::JiraPoller;
pub use linear::LinearPoller;
pub use mixpanel::MixpanelPoller;
pub use openpanel::OpenpanelPoller;
pub use posthog::PosthogPoller;
pub use reddit::RedditPoller;
pub use sentry::SentryPoller;
pub use slack_channels::SlackChannelsPoller;
pub use trello::TrelloPoller;
pub use x::XPoller;
pub use zendesk::ZendeskPoller;

/// Shared HTTP client: 30s request timeout, nothing vendor-specific.
pub(crate) fn http_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| FetchError::Config(format!("http client: {e}")))
}

/// Send a request; non-2xx becomes [`FetchError::Api`] carrying the body's
/// message (`message`/`error`/`errors[0]`/`detail` field of a JSON body, else
/// the raw body truncated).
pub(crate) async fn checked_send(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, FetchError> {
    let response = request
        .send()
        .await
        .map_err(|e| FetchError::Transport(e.to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(FetchError::Api {
        status: status.as_u16(),
        message: api_message(&body),
    })
}

fn api_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["message", "error", "detail"] {
            if let Some(message) = value[key].as_str() {
                return message.to_string();
            }
        }
        if let Some(first) = value["errors"][0].as_str() {
            return first.to_string();
        }
    }
    let mut message: String = body.chars().take(200).collect();
    if message.is_empty() {
        message = "no response body".into();
    }
    message
}

/// Parse a checked response's JSON body.
pub(crate) async fn json_body(
    response: reqwest::Response,
) -> Result<serde_json::Value, FetchError> {
    response
        .json()
        .await
        .map_err(|e| FetchError::Transport(format!("invalid JSON response: {e}")))
}

/// Build one adapter envelope: `{endpoint, context, payload}` with the
/// vendor response **verbatim** in `payload`.
pub(crate) fn envelope(
    endpoint: &str,
    context: serde_json::Value,
    payload: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint,
        "context": context,
        "payload": payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_message_prefers_structured_fields_then_truncates_raw() {
        assert_eq!(
            api_message(r#"{"detail":"Invalid token"}"#),
            "Invalid token"
        );
        assert_eq!(api_message(r#"{"message":"nope"}"#), "nope");
        assert_eq!(api_message(r#"{"errors":["Forbidden"]}"#), "Forbidden");
        assert_eq!(api_message("plain text"), "plain text");
        assert_eq!(api_message(""), "no response body");
        assert_eq!(api_message(&"x".repeat(500)).len(), 200);
    }
}
