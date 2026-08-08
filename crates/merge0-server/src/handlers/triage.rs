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

    // Escalation pre-step: dismissals are not forever. A dismissed report
    // whose impact multiplied — or that gained a delegated ticket — returns
    // to the inbox before this run's fresh clustering.
    let warn_already_sent = state.tenant.last_run_budget_exhausted().await?;
    let reopened = state
        .tenant
        .reopen_escalated(state.reopen_factor, started_at)
        .await?;
    for report in &reopened {
        tracing::info!(report = %report.id, "re-opened dismissed report on escalation");
        notify_report(state, report).await;
    }

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

    // The autonomy dial (off unless config/gate.toml enables it): dispatch
    // qualifying Work Orders through the SAME approve flow a human uses —
    // fresh safety verification included. Paused while over budget.
    if state.gate.autonomy.auto_dispatch && !run.budget_exhausted {
        auto_dispatch(state, started_at).await;
    }

    // Notify Slack for reports that reached (or returned to) the inbox and
    // still need a human. The tolerance absorbs Postgres truncating
    // timestamptz to microseconds on the round-trip (a stored created_at
    // can read back sub-µs earlier than the started_at we passed in).
    let cutoff = started_at - chrono::Duration::milliseconds(5);
    let fresh: Vec<_> = state
        .tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await?
        .into_iter()
        .filter(|r| r.created_at >= cutoff)
        .collect();
    for report in &fresh {
        notify_report(state, report).await;
    }

    // One budget warning per exhausted window, not one per run.
    if run.budget_exhausted && !warn_already_sent {
        if let Some(slack) = &state.slack {
            let message = serde_json::json!({
                "text": format!(
                    "Merge0 token budget exhausted ({} tokens/24h) — gate paused, \
                     candidates stay pending until the window rolls.",
                    state.gate.budget.max_tokens_per_day
                ),
            });
            if let Err(e) = slack.post(&message).await {
                tracing::warn!("slack budget warning failed: {e}");
            }
        }
    }
    Ok(run)
}

async fn notify_report(state: &AppState, report: &merge0_signal::Report) {
    if !state.notify_reports {
        return;
    }
    if let Some(slack) = &state.slack {
        let confidence = state
            .tenant
            .work_order(report.id)
            .await
            .ok()
            .flatten()
            .map(|order| order.confidence);
        let message = merge0_slack::report_message(report, confidence, &state.inbox_url);
        if let Err(e) = slack.post(&message).await {
            tracing::warn!("slack notify failed: {e}");
        }
    }
}

/// Dispatch every fresh Work Order at or above the confidence threshold.
/// Failures are logged, never fatal: an auto-dispatch that cannot proceed
/// (safety check, GitHub outage) leaves the report in the inbox for a
/// human — exactly the fallback the dial promises.
async fn auto_dispatch(state: &AppState, started_at: chrono::DateTime<Utc>) {
    let threshold = state.gate.autonomy.min_confidence;
    let cutoff = started_at - chrono::Duration::milliseconds(5);
    let candidates = match state
        .tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await
    {
        Ok(reports) => reports,
        Err(e) => {
            tracing::warn!("auto-dispatch listing failed: {e}");
            return;
        }
    };
    for report in candidates.into_iter().filter(|r| r.created_at >= cutoff) {
        let order = match state.tenant.work_order(report.id).await {
            Ok(Some(order)) => order,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(report = %report.id, "auto-dispatch order lookup failed: {e}");
                continue;
            }
        };
        if order.confidence < threshold {
            continue;
        }
        match super::actions::approve(state, report.id, super::actions::DispatchedBy::Auto).await {
            Ok(_) => {
                tracing::info!(
                    report = %report.id,
                    confidence = order.confidence.as_str(),
                    "auto-dispatched above confidence threshold"
                );
            }
            Err(e) => {
                tracing::warn!(
                    report = %report.id,
                    "auto-dispatch failed, report stays in the inbox: {e:?}"
                );
            }
        }
    }
}
