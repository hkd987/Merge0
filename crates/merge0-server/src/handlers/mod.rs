//! HTTP handlers, one module per concern.

pub mod actions;
pub mod broker;
pub mod health;
pub mod ingest;
pub mod metrics;
pub mod onboarding;
pub mod owners;
pub mod registry;
pub mod reports;
pub mod runner;
pub mod safety;
pub mod slack;
pub mod spa;
pub mod telemetry;
pub mod triage;
pub mod vendor_webhooks;
pub mod webhooks;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Uniform error → HTTP mapping.
#[derive(Debug)]
pub enum ApiError {
    Status(StatusCode, String),
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError::Status(StatusCode::BAD_REQUEST, message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::Status(StatusCode::NOT_FOUND, message.into())
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        ApiError::Status(StatusCode::CONFLICT, message.into())
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let ApiError::Status(status, message) = self;
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<merge0_store::StoreError> for ApiError {
    fn from(e: merge0_store::StoreError) -> Self {
        match e {
            merge0_store::StoreError::NotFound(what) => ApiError::not_found(what),
            other => ApiError::internal(other),
        }
    }
}

impl From<StatusCode> for ApiError {
    fn from(status: StatusCode) -> Self {
        ApiError::Status(status, "unauthorized".into())
    }
}

pub(crate) fn parse_report_id(id: &str) -> Result<ulid::Ulid, ApiError> {
    ulid::Ulid::from_string(id).map_err(|_| ApiError::bad_request(format!("bad report id {id:?}")))
}

pub(crate) fn auth_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}
