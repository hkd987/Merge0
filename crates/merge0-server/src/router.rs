//! Route table + auth middleware.

use crate::handlers;
use crate::AppState;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ingest/{source}", post(handlers::ingest::ingest))
        .route("/triage/run", post(handlers::triage::run))
        .route("/reports", get(handlers::reports::list))
        .route("/reports/{id}", get(handlers::reports::detail))
        .route("/reports/{id}/approve", post(handlers::reports::approve))
        .route("/reports/{id}/dismiss", post(handlers::reports::dismiss))
        .route("/runner/callback", post(handlers::runner::callback))
        .route("/webhooks/github", post(handlers::webhooks::github))
        .route("/telemetry", get(handlers::telemetry::snapshot))
        .route("/safety", get(handlers::safety::verify))
        .route("/inbox", get(handlers::inbox::page))
        .route("/slack/digest", post(handlers::slack::digest))
        .with_state(state)
}

/// Bearer-token check for mutating endpoints. `None` configured token means
/// open access (development only — main() warns loudly).
pub fn require_bearer(
    configured: &Option<String>,
    authorization_header: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(expected) = configured else {
        return Ok(());
    };
    let presented = authorization_header
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    // Constant-time comparison; length leak is acceptable for random tokens.
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    if expected.len() == presented.len()
        && expected
            .iter()
            .zip(presented)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_check() {
        let token = Some("secret-token".to_string());
        assert!(require_bearer(&token, Some("Bearer secret-token")).is_ok());
        assert!(require_bearer(&token, Some("Bearer wrong")).is_err());
        assert!(require_bearer(&token, Some("secret-token")).is_err());
        assert!(require_bearer(&token, None).is_err());
        assert!(require_bearer(&None, None).is_ok());
    }
}
