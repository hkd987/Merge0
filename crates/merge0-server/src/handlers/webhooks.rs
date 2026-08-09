//! `POST /webhooks/github` — outcome capture (P0-8) and release context
//! (P0-4). Signature-verified; unverifiable requests are rejected.
//!
//! Idempotency (audit C4), two layers: the `x-github-delivery` id is
//! recorded and duplicates short-circuit; and outcome inserts themselves
//! are unique per (report, kind), so even a replay with a fresh delivery id
//! cannot inflate the merge-rate metric.
//!
//! When a Merge0 PR merges and hardening is enabled (PRD §5c), the
//! hardening pass runs asynchronously: candidates → mechanism synthesis →
//! a separate prevention PR through the same inbox.

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

    // Delivery-id dedupe: GitHub delivers at-least-once.
    if let Some(delivery) = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
    {
        if !state
            .tenant
            .record_webhook_delivery(delivery, Utc::now())
            .await?
        {
            return Ok(Json(serde_json::json!({
                "actions": [],
                "duplicate_delivery": delivery,
            })));
        }
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
                let recorded = state
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
                if recorded {
                    state
                        .tenant
                        .set_report_status(id, ReportStatus::Completed)
                        .await?;
                    actions.push(format!("merged outcome for report {id}"));
                    // PRD §5c: "when a Work Order's PR merges, an
                    // asynchronous hardening pass evaluates…". Flag-gated:
                    // ships after the Phase 0 gate is met.
                    if state.hardening_enabled {
                        spawn_hardening_pass(state.clone());
                        actions.push("hardening pass queued".into());
                    }
                } else {
                    actions.push(format!("duplicate merged outcome for report {id} ignored"));
                }
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
                let recorded = state
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
                if recorded {
                    state
                        .tenant
                        .set_report_status(id, ReportStatus::Completed)
                        .await?;
                    actions.push(format!("closed outcome for report {id}"));
                }
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

/// Run the hardening pass in the background: one prevention proposal per
/// eligible fingerprint, each landing as a `[hardening]` PR + inbox report.
/// Errors are logged, never surfaced to the webhook response (GitHub would
/// just retry).
pub(crate) fn spawn_hardening_pass(state: AppState) {
    tokio::spawn(async move {
        let now = Utc::now();
        let candidates = match merge0_hardening::find_candidates(&state.tenant).await {
            Ok(candidates) => candidates,
            Err(e) => {
                tracing::error!("hardening: candidate search failed: {e}");
                return;
            }
        };
        for candidate in candidates {
            let signal = match state
                .tenant
                .signal_by_fingerprint(&candidate.fingerprint)
                .await
            {
                Ok(Some(signal)) => signal,
                Ok(None) => continue,
                Err(e) => {
                    tracing::error!("hardening: signal load failed: {e}");
                    continue;
                }
            };
            let mechanism = merge0_hardening::synthesize(&candidate, &signal);
            let intent_doc = state
                .github
                .get_file_content(&state.repo, crate::intent::INTENT_DOC_PATH)
                .await
                .ok()
                .flatten();
            match merge0_hardening::propose(
                &candidate,
                &mechanism,
                state.github.as_ref(),
                &state.repo,
                &state.tenant,
                intent_doc.as_deref(),
                now,
            )
            .await
            {
                Ok(proposal) => {
                    tracing::info!(
                        "hardening: proposed {} for fingerprint {}",
                        proposal.pr.url,
                        candidate.fingerprint
                    );
                }
                Err(e) => tracing::error!(
                    "hardening: proposal failed for {}: {e}",
                    candidate.fingerprint
                ),
            }
        }
    });
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
    let recorded = state
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
    Ok(if recorded {
        vec![format!("revert recorded for report {id}")]
    } else {
        vec![format!("duplicate revert for report {id} ignored")]
    })
}
