//! `POST /slack/digest` — the weekly digest (PRD §6): pending reports,
//! PRs awaiting review with staleness, merged count. Cron-able; manual
//! trigger keeps it testable.

use super::ingest::auth_header;
use super::ApiError;
use crate::router::require_bearer;
use crate::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use merge0_signal::ReportStatus;

pub async fn digest(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_bearer(&state.api_token, auth_header(&headers))?;
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
