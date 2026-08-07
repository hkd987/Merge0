//! `GET /healthz` — a real readiness probe (audit O4): reports healthy only
//! when the database answers. No data beyond up/down.

use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

pub async fn healthz(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match state.tenant.ping().await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => {
            tracing::error!("healthz: database unreachable: {e}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "ok": false, "database": "unreachable" })),
            )
        }
    }
}
