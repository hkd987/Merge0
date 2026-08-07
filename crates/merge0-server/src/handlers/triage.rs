//! `POST /triage/run` — one full triage pass, returning the run summary.
//! (The scheduler in `main` calls the same logic on its interval; the
//! endpoint keeps runs manually triggerable and e2e-testable.)
//!
//! The customer's MERGE0.md is fetched from their repo for THIS run
//! (PRD §3, audit C5): editing intent docs takes effect immediately.

use super::ApiError;
use crate::{intent, AppState};
use axum::extract::State;
use axum::Json;
use chrono::Utc;
use merge0_signal::ReportStatus;
use merge0_triage::pipeline::run_triage;

pub async fn run(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let run = run_once(&state).await.map_err(ApiError::internal)?;
    Ok(Json(serde_json::to_value(&run).expect("run serializes")))
}

/// Shared by the HTTP endpoint and the interval scheduler.
pub async fn run_once(
    state: &AppState,
) -> Result<merge0_triage::pipeline::TriageRun, Box<dyn std::error::Error + Send + Sync>> {
    let started_at = Utc::now();
    let intent_text = intent::resolve_intent(state).await;
    let run = run_triage(
        &state.tenant,
        state.model.as_ref(),
        &state.scouts,
        &state.gate,
        &intent_text,
        &state.repo.full(),
        started_at,
    )
    .await?;

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
    Ok(run)
}
