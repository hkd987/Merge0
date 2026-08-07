//! `POST /webhooks/{vendor}` — NATIVE vendor webhooks (audit C1): Sentry,
//! PostHog, Zendesk, and Datadog can push directly, no fetch-layer poll
//! required. Each vendor's own authentication scheme is verified (Sentry's
//! HMAC signature, Zendesk's timestamped signature, shared tokens for
//! vendors without a signing scheme), then the payload is converted into
//! the adapter envelope by the fetch layer's builders and normalized by the
//! ordinary adapters.

use super::{ingest::adapter_for, ApiError};
use crate::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use merge0_fetch::webhooks as vw;
use merge0_store::IngestOutcome;

pub async fn receive(
    State(state): State<AppState>,
    Path(vendor): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = &state.vendor_webhooks;
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let unauthorized = || ApiError::Status(StatusCode::UNAUTHORIZED, "bad vendor signature".into());
    let unconfigured = || {
        ApiError::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "vendor webhook secret not configured".into(),
        )
    };

    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("bad JSON: {e}")))?;

    // Verify with the vendor's own scheme, then build the adapter envelope.
    let (envelope, source) = match vendor.as_str() {
        "sentry" => {
            let secret = config
                .sentry_client_secret
                .as_ref()
                .ok_or_else(unconfigured)?;
            if !vw::verify_sentry_signature(secret, &body, header("sentry-hook-signature")) {
                return Err(unauthorized());
            }
            (vw::sentry_webhook_to_envelope(&payload), "sentry")
        }
        "posthog" => {
            let token = config
                .posthog_shared_token
                .as_ref()
                .ok_or_else(unconfigured)?;
            if !vw::verify_shared_token(token, header("x-merge0-webhook-token")) {
                return Err(unauthorized());
            }
            (
                vw::posthog_webhook_to_envelope(&payload, &config.posthog_project_base_url),
                "posthog",
            )
        }
        "zendesk" => {
            let secret = config
                .zendesk_signing_secret
                .as_ref()
                .ok_or_else(unconfigured)?;
            let timestamp = header("x-zendesk-webhook-signature-timestamp");
            if !vw::verify_zendesk_signature(
                secret,
                timestamp,
                &body,
                header("x-zendesk-webhook-signature"),
            ) {
                return Err(unauthorized());
            }
            (
                vw::zendesk_webhook_to_envelope(&payload, &config.zendesk_agent_base_url),
                "zendesk",
            )
        }
        "datadog" => {
            let token = config
                .datadog_shared_token
                .as_ref()
                .ok_or_else(unconfigured)?;
            if !vw::verify_shared_token(token, header("x-merge0-webhook-token")) {
                return Err(unauthorized());
            }
            (
                vw::datadog_webhook_to_envelope(&payload, &config.datadog_app_base_url),
                "datadog",
            )
        }
        other => return Err(ApiError::not_found(format!("unknown vendor {other:?}"))),
    };

    let Some(envelope) = envelope else {
        return Err(ApiError::bad_request(format!(
            "unrecognized {vendor} webhook payload shape"
        )));
    };
    let adapter = adapter_for(source).expect("vendor sources have adapters");
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
        "vendor": vendor,
        "received": signals.len(),
        "inserted": inserted,
        "updated": updated,
    })))
}
