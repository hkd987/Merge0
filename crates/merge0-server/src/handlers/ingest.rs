//! Ingestion: `POST /ingest/{source}` with an adapter envelope body.
//!
//! The source name selects the adapter; the body is the adapter's envelope
//! (endpoint + context + verbatim vendor payload). Signals are upserted with
//! fingerprint dedupe. Auth is enforced by the router middleware. This is
//! the push-side surface; the pull side is the fetch layer's pollers.

use super::ApiError;
use crate::AppState;
use axum::extract::{Path, State};
use axum::Json;
use merge0_adapters::Adapter;
use merge0_store::IngestOutcome;

pub(crate) fn adapter_for(source: &str) -> Option<Box<dyn Adapter>> {
    Some(match source {
        "posthog" => Box::new(merge0_adapter_posthog::PosthogAdapter),
        "sentry" => Box::new(merge0_adapter_sentry::SentryAdapter),
        "zendesk" => Box::new(merge0_adapter_zendesk::ZendeskAdapter),
        "github-issues" => Box::new(merge0_adapter_github_issues::GithubIssuesAdapter),
        "webhook" => Box::new(merge0_adapter_webhook::WebhookAdapter),
        "otel" => Box::new(merge0_adapter_webhook::OtelAdapter),
        "datadog" => Box::new(merge0_adapter_datadog::DatadogAdapter),
        "loopforge" => Box::new(merge0_adapter_loopforge::LoopforgeAdapter),
        "jira" => Box::new(merge0_adapter_jira::JiraAdapter),
        "linear" => Box::new(merge0_adapter_linear::LinearAdapter),
        "slack" => Box::new(merge0_adapter_slack::SlackAdapter),
        "asana" => Box::new(merge0_adapter_asana::AsanaAdapter),
        "trello" => Box::new(merge0_adapter_trello::TrelloAdapter),
        "intercom" => Box::new(merge0_adapter_intercom::IntercomAdapter),
        "mixpanel" => Box::new(merge0_adapter_mixpanel::MixpanelAdapter),
        "openpanel" => Box::new(merge0_adapter_openpanel::OpenpanelAdapter),
        "meta" => Box::new(merge0_meta::MetaAdapter),
        _ => return None,
    })
}

pub async fn ingest(
    State(state): State<AppState>,
    Path(source): Path<String>,
    Json(envelope): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(adapter) = adapter_for(&source) else {
        return Err(ApiError::not_found(format!("unknown source {source:?}")));
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
