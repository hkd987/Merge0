//! `POST /triage/run` — one full triage pass, returning the run summary.
//! (The scheduler in `main` calls this same logic on its interval; the
//! endpoint keeps runs manually triggerable and e2e-testable.)

use super::ingest::auth_header;
use super::ApiError;
use crate::router::require_bearer;
use crate::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use merge0_signal::ReportStatus;
use merge0_triage::pipeline::run_triage;

pub async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_bearer(&state.api_token, auth_header(&headers))?;
    let started_at = Utc::now();
    let run = run_triage(
        &state.tenant,
        state.model.as_ref(),
        &state.scouts,
        &state.gate,
        &state.intent_text,
        &state.repo,
        started_at,
    )
    .await
    .map_err(ApiError::internal)?;

    // Notify Slack for reports that just reached the inbox. The tolerance
    // absorbs Postgres truncating timestamptz to microseconds on the
    // round-trip (a stored created_at can read back sub-µs earlier than the
    // started_at we passed in).
    if let Some(slack) = &state.slack {
        let cutoff = started_at - chrono::Duration::milliseconds(5);
        let fresh = state
            .tenant
            .list_reports(Some(ReportStatus::AwaitingReview))
            .await?
            .into_iter()
            .filter(|r| r.created_at >= cutoff);
        for report in fresh {
            let message = merge0_slack::report_message(&report, &state.inbox_url);
            if let Err(e) = slack.post(&message).await {
                tracing::warn!("slack notify failed: {e}");
            }
        }
    }

    Ok(Json(serde_json::to_value(&run).expect("run serializes")))
}
