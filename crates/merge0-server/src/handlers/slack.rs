//! Slack surface: the weekly digest (PRD §6) and — audit finding M1 — the
//! interaction endpoint that makes the Approve/Dismiss buttons in Slack
//! messages actually work. Interactions carry Slack's request signature;
//! verdicts route through the same shared actions as the web inbox.

use super::{actions, parse_report_id, ApiError};
use crate::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use merge0_signal::ReportStatus;
use merge0_slack::{parse_interaction, verify_slack_signature, Verdict};

pub async fn digest(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = state
        .tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await?;
    let now = Utc::now();
    let mut prs_awaiting: Vec<(String, i64)> = Vec::new();
    for report in state
        .tenant
        .list_reports(Some(ReportStatus::PrOpen))
        .await?
    {
        if let Some(dispatch) = state.tenant.dispatch(report.id).await? {
            if let (Some(pr_url), Some(opened_at)) = (dispatch.pr_url, dispatch.pr_opened_at) {
                prs_awaiting.push((pr_url, (now - opened_at).num_days()));
            }
        }
    }
    let merged_this_week = state.tenant.telemetry(7, now).await?.counts.prs_merged;

    let message = merge0_slack::weekly_digest(&pending, &prs_awaiting, merged_this_week);
    if let Some(slack) = &state.slack {
        slack.post(&message).await.map_err(ApiError::internal)?;
    }
    Ok(Json(message))
}

/// `POST /slack/interactions` — Slack posts here when a reviewer clicks
/// Approve or picks a dismissal reason. Signature-verified (v0 scheme),
/// then routed through the shared action logic.
pub async fn interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(signing_secret) = &state.slack_signing_secret else {
        return Err(ApiError::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "slack signing secret not configured".into(),
        ));
    };
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let body_text =
        std::str::from_utf8(&body).map_err(|_| ApiError::bad_request("body is not UTF-8"))?;
    if !verify_slack_signature(signing_secret, timestamp, body_text, signature, Utc::now()) {
        return Err(ApiError::Status(
            StatusCode::UNAUTHORIZED,
            "bad slack signature".into(),
        ));
    }

    let verdict = parse_interaction(body_text)
        .map_err(|e| ApiError::bad_request(format!("unparseable interaction: {e}")))?;
    let id = parse_report_id(&verdict.report_id)?;
    let result = match verdict.verdict {
        Verdict::Approve => actions::approve(&state, id).await?,
        Verdict::Dismiss(reason) => actions::dismiss(&state, id, reason).await?,
    };
    Ok(Json(result))
}
