//! `POST /webhooks/github` — outcome capture (P0-8) and release context
//! (P0-4). Signature-verified; unverifiable requests are rejected.

use super::ApiError;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use merge0_github::webhook::{
    parse, reverted_sha, verify_signature, within_revert_window, WebhookEvent,
};
use merge0_signal::{OutcomeKind, ReportStatus};

pub async fn github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(secret) = &state.webhook_secret else {
        return Err(ApiError::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook secret not configured".into(),
        ));
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(secret, &body, signature) {
        return Err(ApiError::Status(
            StatusCode::UNAUTHORIZED,
            "bad webhook signature".into(),
        ));
    }
    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("bad JSON: {e}")))?;
    let event = parse(event_name, &payload).map_err(|e| ApiError::bad_request(e.to_string()))?;

    let mut actions: Vec<String> = Vec::new();
    match event {
        WebhookEvent::PrMerged {
            pr_url,
            merged_at,
            merge_sha,
            title,
            body: pr_body,
        } => {
            if let Some(id) = state.tenant.report_for_pr(&pr_url).await? {
                if let Some(sha) = &merge_sha {
                    state.tenant.record_merge_sha(id, sha).await?;
                }
                let tokens = state
                    .tenant
                    .dispatch(id)
                    .await?
                    .and_then(|d| d.tokens_spent);
                state
                    .tenant
                    .record_outcome(
                        id,
                        OutcomeKind::Merged,
                        Some(&pr_url),
                        merged_at,
                        None,
                        tokens,
                    )
                    .await?;
                state
                    .tenant
                    .set_report_status(id, ReportStatus::Completed)
                    .await?;
                actions.push(format!("merged outcome for report {id}"));
            }
            // A merged *revert PR* is also a revert of the original.
            if title.starts_with("Revert") {
                if let Some(sha) = reverted_sha(&pr_body) {
                    actions.extend(record_revert(&state, &sha, merged_at).await?);
                }
            }
        }
        WebhookEvent::PrClosed { pr_url, closed_at } => {
            if let Some(id) = state.tenant.report_for_pr(&pr_url).await? {
                state
                    .tenant
                    .record_outcome(
                        id,
                        OutcomeKind::Closed,
                        Some(&pr_url),
                        closed_at,
                        None,
                        None,
                    )
                    .await?;
                state
                    .tenant
                    .set_report_status(id, ReportStatus::Completed)
                    .await?;
                actions.push(format!("closed outcome for report {id}"));
            }
        }
        WebhookEvent::Release {
            tag,
            published_at,
            sha,
            notes,
        } => {
            state
                .tenant
                .upsert_release(&tag, sha.as_deref(), published_at, notes.as_deref())
                .await?;
            actions.push(format!("release {tag} recorded"));
        }
        WebhookEvent::Push { commits } => {
            for commit in &commits {
                if let Some(sha) = reverted_sha(&commit.message) {
                    let at = commit.timestamp.unwrap_or_else(Utc::now);
                    actions.extend(record_revert(&state, &sha, at).await?);
                }
            }
        }
        WebhookEvent::Other => {}
    }

    Ok(Json(serde_json::json!({ "actions": actions })))
}

/// Map a reverted commit SHA back to the originating Merge0 PR and record
/// the hard negative (P0-8: within 14 days).
async fn record_revert(
    state: &AppState,
    sha: &str,
    reverted_at: chrono::DateTime<Utc>,
) -> Result<Vec<String>, ApiError> {
    let Some(id) = state.tenant.report_for_merge_sha(sha).await? else {
        return Ok(vec![]);
    };
    let merged_at = state
        .tenant
        .outcomes_for_report(id)
        .await?
        .into_iter()
        .find(|o| o.outcome == OutcomeKind::Merged)
        .map(|o| o.occurred_at);
    let Some(merged_at) = merged_at else {
        return Ok(vec![]);
    };
    if !within_revert_window(merged_at, reverted_at) {
        return Ok(vec![format!(
            "revert of report {id} outside the {}-day window — not recorded",
            merge0_github::webhook::REVERT_WINDOW_DAYS
        )]);
    }
    state
        .tenant
        .record_outcome(
            id,
            OutcomeKind::Reverted,
            None,
            reverted_at,
            Some(&format!("hard negative: merge commit {sha} reverted")),
            None,
        )
        .await?;
    Ok(vec![format!("revert recorded for report {id}")])
}
