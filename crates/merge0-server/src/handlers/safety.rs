//! `GET /safety` — onboarding verification status (P0-9).

use super::ApiError;
use crate::AppState;
use axum::extract::State;
use axum::Json;
use merge0_github::safety::verify_repo_safety;
use merge0_github::RepoRef;

pub async fn verify(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let repo = RepoRef::parse(&state.repo).map_err(ApiError::internal)?;
    let report = verify_repo_safety(state.github.as_ref(), &repo)
        .await
        .map_err(ApiError::internal)?;
    let satisfied = report.satisfied();
    let failures = report.failures();
    Ok(Json(serde_json::json!({
        "repo": state.repo,
        "report": report,
        "satisfied": satisfied,
        "failures": failures,
    })))
}
