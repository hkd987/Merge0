//! `GET /telemetry` — the internal acceptance-rate dashboard (P0-10),
//! computed continuously from the first PR.

use super::ApiError;
use crate::AppState;
use axum::extract::{Query, State};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Params {
    pub window_days: Option<u32>,
}

pub async fn snapshot(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let window = params.window_days.unwrap_or(30).clamp(1, 365);
    let snapshot = state
        .tenant
        .telemetry(window, state.efficacy_grace_days, Utc::now())
        .await?;
    Ok(Json(serde_json::to_value(&snapshot).expect("serializes")))
}
