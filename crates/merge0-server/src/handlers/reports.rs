//! The inbox API (P0-7): list, detail, approve → dispatch, dismiss.

use super::ingest::auth_header;
use super::{parse_report_id, ApiError};
use crate::router::require_bearer;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use merge0_github::safety::verify_repo_safety;
use merge0_github::RepoRef;
use merge0_runner::{ActionsRunner, Runner};
use merge0_signal::{DismissReason, ReportStatus};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListParams {
    pub status: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let status = params
        .status
        .as_deref()
        .map(|s| {
            serde_json::from_value::<ReportStatus>(serde_json::Value::String(s.to_string()))
                .map_err(|_| ApiError::bad_request(format!("unknown status {s:?}")))
        })
        .transpose()?;
    let reports = state.tenant.list_reports(status).await?;
    Ok(Json(serde_json::to_value(&reports).expect("serializes")))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = parse_report_id(&id)?;
    let report = state.tenant.get_report(id).await?;
    let gate_decision = state.tenant.gate_decision(id).await?;
    let work_order = state.tenant.work_order(id).await?;
    let dispatch = state.tenant.dispatch(id).await?;
    let outcomes = state.tenant.outcomes_for_report(id).await?;
    let handoff_brief = state.tenant.handoff_brief(id).await?;
    Ok(Json(serde_json::json!({
        "report": report,
        "gate_decision": gate_decision,
        "work_order": work_order,
        "dispatch": dispatch.map(|d| serde_json::json!({
            "runner_kind": d.runner_kind,
            "dispatched_at": d.dispatched_at,
            "status": d.status,
            "pr_url": d.pr_url,
            "branch": d.branch,
            "discard_reason": d.discard_reason,
            "diagnosis": d.diagnosis,
            "tokens_spent": d.tokens_spent,
        })),
        "outcomes": outcomes,
        "handoff_brief": handoff_brief,
    })))
}

/// Approve → verify safety (P0-9) → dispatch (P0-6).
pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_bearer(&state.api_token, auth_header(&headers))?;
    let id = parse_report_id(&id)?;

    let report = state.tenant.get_report(id).await?;
    if report.status != ReportStatus::AwaitingReview {
        return Err(ApiError::conflict(format!(
            "report is {:?}, only awaiting_review reports can be approved",
            report.status
        )));
    }
    let Some(work_order) = state.tenant.work_order(id).await? else {
        return Err(ApiError::conflict("report has no work order"));
    };

    // P0-9: refuse to dispatch until branch protection + required CI are
    // confirmed — verified fresh at every approval, not cached from
    // onboarding.
    let repo = RepoRef::parse(&state.repo).map_err(ApiError::internal)?;
    let safety = verify_repo_safety(state.github.as_ref(), &repo)
        .await
        .map_err(ApiError::internal)?;
    if !safety.satisfied() {
        return Err(ApiError::conflict(format!(
            "safety verification failed: {}",
            safety.failures().join("; ")
        )));
    }

    let now = Utc::now();
    state.tenant.approve_report(id, now).await?;
    let runner = ActionsRunner {
        api: state.github.clone(),
        agent: state.agent.clone(),
        callback_url: state.callback_url.clone(),
    };
    let receipt = runner
        .dispatch(&work_order)
        .await
        .map_err(ApiError::internal)?;
    state
        .tenant
        .record_dispatch(id, &receipt.runner_kind, now)
        .await?;
    state
        .tenant
        .set_report_status(id, ReportStatus::Dispatched)
        .await?;

    Ok(Json(serde_json::json!({
        "approved": id.to_string(),
        "dispatched_to": receipt.repo.full(),
        "runner": receipt.runner_kind,
    })))
}

#[derive(Deserialize)]
pub struct DismissBody {
    pub reason: DismissReason,
}

pub async fn dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Auth strictly before parsing (see runner callback).
    require_bearer(&state.api_token, auth_header(&headers))?;
    let body: DismissBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("bad dismiss body: {e}")))?;
    let id = parse_report_id(&id)?;
    state
        .tenant
        .dismiss_report(id, body.reason, Utc::now())
        .await?;
    Ok(Json(serde_json::json!({
        "dismissed": id.to_string(),
        "reason": body.reason,
    })))
}
