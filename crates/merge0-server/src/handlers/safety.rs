//! `GET /safety` — onboarding verification status (P0-9). Authenticated:
//! this reveals the customer repo's branch-protection posture (audit C2).

use super::ApiError;
use crate::AppState;
use axum::extract::State;
use axum::Json;
use merge0_github::safety::verify_repo_safety;

pub async fn verify(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let report = verify_repo_safety(state.github.as_ref(), &state.repo)
        .await
        .map_err(ApiError::internal)?;
    let satisfied = report.satisfied();
    let failures = report.failures();
    Ok(Json(serde_json::json!({
        "repo": state.repo.full(),
        "report": report,
        "satisfied": satisfied,
        "failures": failures,
    })))
}
