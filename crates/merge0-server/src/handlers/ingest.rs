//! Ingestion: `POST /ingest/{source}` with an adapter envelope body.
//!
//! The source name selects the adapter; the body is the adapter's envelope
//! (endpoint + context + verbatim vendor payload). Signals are upserted with
//! fingerprint dedupe.

use super::ApiError;
use crate::router::require_bearer;
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use merge0_adapters::Adapter;
use merge0_store::IngestOutcome;

pub async fn ingest(
    State(state): State<AppState>,
    Path(source): Path<String>,
    headers: HeaderMap,
    Json(envelope): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_bearer(&state.api_token, auth_header(&headers))?;

    let adapter: Box<dyn Adapter> = match source.as_str() {
        "posthog" => Box::new(merge0_adapter_posthog::PosthogAdapter),
        "sentry" => Box::new(merge0_adapter_sentry::SentryAdapter),
        "zendesk" => Box::new(merge0_adapter_zendesk::ZendeskAdapter),
        "github-issues" => Box::new(merge0_adapter_github_issues::GithubIssuesAdapter),
        "webhook" => Box::new(merge0_adapter_webhook::WebhookAdapter),
        "otel" => Box::new(merge0_adapter_webhook::OtelAdapter),
        "datadog" => Box::new(merge0_adapter_datadog::DatadogAdapter),
        "loopforge" => Box::new(merge0_adapter_loopforge::LoopforgeAdapter),
        other => return Err(ApiError::not_found(format!("unknown source {other:?}"))),
    };

    let signals = adapter
        .normalize(&envelope)
        .map_err(|e| ApiError::bad_request(format!("adapter rejected payload: {e}")))?;

    let mut inserted = 0u64;
    let mut updated = 0u64;
    for signal in &signals {
        match state.tenant.upsert_signal(signal).await? {
            IngestOutcome::Inserted => inserted += 1,
            IngestOutcome::Updated => updated += 1,
        }
    }
    Ok(Json(serde_json::json!({
        "source": source,
        "received": signals.len(),
        "inserted": inserted,
        "updated": updated,
    })))
}

pub(crate) fn auth_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}
