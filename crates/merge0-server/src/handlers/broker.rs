//! `POST /broker/credentials` — the credential broker's HTTP face (PRD §5a,
//! P2): a runner exchanges its per-tenant key plus an approved Work Order
//! reference for a short-lived, single-repo, single-use credential.
//!
//! Grants are registered by the approve path at dispatch time; the broker
//! itself enforces authentication-first checking, repo scope, and
//! single-use consumption. Mounted on the OPEN router — like the runner
//! callback, the request self-authenticates (the runner key), and auth runs
//! strictly before body parsing.

use super::ApiError;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{Duration, Utc};
use merge0_broker::BrokerError;
use serde::Deserialize;

#[derive(Deserialize)]
struct CredentialRequest {
    work_order_id: String,
    repo: String,
    /// Clamped to the broker's TTL cap server-side.
    #[serde(default = "default_ttl_minutes")]
    ttl_minutes: i64,
}

fn default_ttl_minutes() -> i64 {
    10
}

pub async fn credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(broker) = &state.broker else {
        return Err(ApiError::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential broker not configured".into(),
        ));
    };
    // Authentication strictly before parsing — and it has to be *checked*
    // here, not merely extracted. Reading the key and then parsing the body
    // before `request_credentials` validates it left an unauthenticated
    // caller able to drive JSON parsing on this endpoint, which is exactly
    // what this comment claimed was impossible.
    let presented = super::auth_header(&headers)
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    if !broker.lock().await.authenticates(&presented) {
        return Err(ApiError::Status(
            StatusCode::UNAUTHORIZED,
            "invalid runner key".into(),
        ));
    }

    let request: CredentialRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("bad credential request: {e}")))?;

    let now = Utc::now();
    let outcome = broker.lock().await.request_credentials(
        &presented,
        &request.work_order_id,
        &request.repo,
        Duration::minutes(request.ttl_minutes.max(0)),
        now,
    );
    match outcome {
        Ok(credential) => Ok(Json(serde_json::json!({
            "token": credential.token.expose_for_credential_helper(),
            "expires_at": credential.expires_at,
        }))),
        Err(BrokerError::InvalidRunnerKey) => Err(ApiError::Status(
            StatusCode::UNAUTHORIZED,
            "invalid runner key".into(),
        )),
        Err(e @ (BrokerError::NoGrant(_) | BrokerError::RepoMismatch { .. })) => {
            Err(ApiError::Status(StatusCode::FORBIDDEN, e.to_string()))
        }
        Err(e @ BrokerError::GrantConsumed(_)) => Err(ApiError::conflict(e.to_string())),
        Err(e) => Err(ApiError::internal(e.to_string())),
    }
}
