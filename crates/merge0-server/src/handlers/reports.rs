//! The inbox API (P0-7): list, detail, approve → dispatch, dismiss. The
//! action logic itself lives in [`super::actions`], shared with the Slack
//! interaction surface.

use super::{actions, parse_report_id, ApiError};
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
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
            "extensions": d.extensions,
        })),
        "outcomes": outcomes,
        "handoff_brief": handoff_brief,
    })))
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = parse_report_id(&id)?;
    actions::approve(&state, id).await.map(Json)
}

#[derive(Deserialize)]
pub struct DismissBody {
    pub reason: DismissReason,
}

pub async fn dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DismissBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = parse_report_id(&id)?;
    actions::dismiss(&state, id, body.reason).await.map(Json)
}
