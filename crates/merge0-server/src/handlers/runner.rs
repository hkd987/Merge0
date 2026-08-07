//! `POST /runner/callback` — the customer workflow reports its run.
//!
//! Budgets are re-enforced server-side (`enforce_budgets`): the runner's
//! self-report is checked, not trusted. No red PRs reach the inbox (P0-6);
//! failed runs write a failure outcome and open nothing. Retried callbacks
//! (Actions re-runs, network retries) are detected via the dispatch status
//! and produce no duplicate outcomes or notifications (audit C4).

use super::{auth_header, parse_report_id, ApiError};
use crate::router::require_bearer;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use merge0_runner::{enforce_budgets, RunReport, RunStatus};
use merge0_signal::ReportStatus;
use merge0_store::DispatchStatus;

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Auth strictly before parsing: unauthenticated callers learn nothing
    // about the expected body shape.
    require_bearer(&state.runner_token, auth_header(&headers))?;
    let report: RunReport = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("bad run report: {e}")))?;
    let id = parse_report_id(&report.report_id)?;
    let Some(order) = state.tenant.work_order(id).await? else {
        return Err(ApiError::not_found(format!(
            "no work order for report {id}"
        )));
    };
    let Some(dispatch) = state.tenant.dispatch(id).await? else {
        return Err(ApiError::conflict("report was never dispatched"));
    };
    // Idempotency: only a Dispatched run can transition. A retried callback
    // finds the dispatch already terminal and becomes a recorded no-op.
    if dispatch.status != DispatchStatus::Dispatched {
        return Ok(Json(serde_json::json!({
            "recorded": id.to_string(),
            "duplicate": true,
            "current_status": dispatch.status,
        })));
    }

    let report = enforce_budgets(&order, report);
    let now = Utc::now();
    match report.status {
        RunStatus::Opened => {
            let pr_url = report.pr_url.as_deref().expect("enforced by budgets");
            state
                .tenant
                .record_pr_opened(
                    id,
                    pr_url,
                    report.branch.as_deref().unwrap_or_default(),
                    now,
                    report.tokens_spent,
                    report.extensions.clone(),
                )
                .await?;
            state
                .tenant
                .set_report_status(id, ReportStatus::PrOpen)
                .await?;
            if let Some(slack) = &state.slack {
                let full = state.tenant.get_report(id).await?;
                let message = merge0_slack::pr_ready_message(&full, pr_url);
                if let Err(e) = slack.post(&message).await {
                    tracing::warn!("slack notify failed: {e}");
                }
            }
        }
        RunStatus::Discarded => {
            state
                .tenant
                .record_discard(
                    id,
                    report.discard_reason.as_deref().unwrap_or("discarded"),
                    report
                        .diagnosis
                        .as_deref()
                        .unwrap_or("no diagnosis reported"),
                    now,
                    report.tokens_spent,
                )
                .await?;
            state
                .tenant
                .set_report_status(id, ReportStatus::Completed)
                .await?;
        }
        RunStatus::Failed => {
            // Infrastructure failure: recorded like a discard, with the
            // reason marking it as infra rather than a self-discard.
            state
                .tenant
                .record_discard(
                    id,
                    &format!(
                        "runner failure: {}",
                        report.discard_reason.as_deref().unwrap_or("unknown")
                    ),
                    report.diagnosis.as_deref().unwrap_or("no diagnosis"),
                    now,
                    report.tokens_spent,
                )
                .await?;
            state
                .tenant
                .set_report_status(id, ReportStatus::Completed)
                .await?;
        }
    }

    Ok(Json(serde_json::json!({
        "recorded": id.to_string(),
        "status": report.status,
        "coerced_reason": report.discard_reason,
    })))
}
