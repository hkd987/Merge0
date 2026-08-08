//! Route table, auth middleware, and protective layers.
//!
//! Auth model (audit finding C2): EVERY route requires the API bearer token
//! via router-level middleware, except the endpoints that carry their own
//! authentication (GitHub webhook HMAC, runner callback token, Slack
//! request signature) and two deliberately public surfaces: `/healthz`
//! (orchestrator probe, no data) and `GET /inbox` (a static shell page
//! containing NO report data — it fetches everything client-side with the
//! bearer token).

use crate::handlers;
use crate::AppState;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;

pub fn app(state: AppState) -> Router {
    let protected = Router::new()
        .route("/ingest/{source}", post(handlers::ingest::ingest))
        .route("/triage/run", post(handlers::triage::run))
        .route("/reports", get(handlers::reports::list))
        .route("/reports/{id}", get(handlers::reports::detail))
        .route("/reports/{id}/approve", post(handlers::reports::approve))
        .route("/reports/{id}/dismiss", post(handlers::reports::dismiss))
        .route("/telemetry", get(handlers::telemetry::snapshot))
        .route("/metrics", get(handlers::metrics::scrape))
        .route("/safety", get(handlers::safety::verify))
        .route("/onboarding", get(handlers::onboarding::bundle))
        .route("/registry/skills", get(handlers::registry::list))
        .route(
            "/registry/skills/{name}/install",
            post(handlers::registry::install),
        )
        .route("/slack/digest", post(handlers::slack::digest))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_token,
        ));

    // Self-authenticated or data-free routes. (`/webhooks/github` is a
    // static route and takes precedence over the `{vendor}` capture.)
    // Rate-limited per IP: these verify their own signatures, but the
    // verification itself must not be a free DoS vector. The SPA routes
    // serve embedded, data-free static assets (auth is client-side —
    // audit C2).
    let open = Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::spa::serve))
        .route("/inbox", get(handlers::spa::serve))
        .route("/dashboard", get(handlers::spa::serve))
        .route("/setup", get(handlers::spa::serve))
        .route("/assets/{*file}", get(handlers::spa::serve))
        .route("/runner/callback", post(handlers::runner::callback))
        .route("/broker/credentials", post(handlers::broker::credentials))
        .route("/webhooks/github", post(handlers::webhooks::github))
        .route(
            "/webhooks/{vendor}",
            post(handlers::vendor_webhooks::receive),
        )
        .route("/slack/interactions", post(handlers::slack::interactions))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_open_routes,
        ));

    protected
        .merge(open)
        .layer(DefaultBodyLimit::max(5 * 1024 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Per-IP token-bucket check for the open surface. The peer IP comes from
/// connect info when the server is built with it (main() is); without it,
/// all peers share one bucket — limiting fails toward closed, not open.
async fn rate_limit_open_routes(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(limiter) = &state.rate_limiter {
        let ip = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|info| info.0.ip());
        if !limiter.allow(ip) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }
    Ok(next.run(request).await)
}

/// Router-level bearer check for the protected API surface.
async fn require_api_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    require_bearer(&state.api_token, header)?;
    Ok(next.run(request).await)
}

/// Bearer-token check. `None` configured token means open access
/// (development only — main() warns loudly).
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
