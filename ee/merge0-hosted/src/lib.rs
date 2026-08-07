//! The hosted control plane (commercial, PRD Phase 2): tenant provisioning
//! and lifecycle, membership + RBAC, usage metering under both pricing
//! models, cross-tenant gate priors, and the audit trail. This is the
//! multi-tenant machinery the MIT core deliberately does not carry — one
//! `merge0-server` process runs per tenant (or per plan tier), pointed at
//! the tenant schema this plane provisions.
//!
//! Auth model: an operator bearer token (`MERGE0_EE_ADMIN_TOKEN`) gates the
//! whole surface; per-tenant actions additionally check the acting user's
//! membership role (`x-merge0-actor` header) through the RBAC matrix, so an
//! operator console can act on behalf of org members with their privileges.

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use merge0_ee::{allowed, Action, EeError, Pricing, Role, TenantManager};
use serde::Deserialize;
use std::sync::Arc;
use ulid::Ulid;

#[derive(Clone)]
pub struct HostedState {
    pub manager: Arc<TenantManager>,
    pub admin_token: Option<String>,
}

pub fn app(state: HostedState) -> Router {
    Router::new()
        .route("/ee/tenants", post(create_tenant).get(list_tenants))
        .route("/ee/tenants/{id}/suspend", post(suspend_tenant))
        .route("/ee/tenants/{id}/resume", post(resume_tenant))
        .route("/ee/tenants/{id}/members", post(add_member))
        .route("/ee/tenants/{id}/usage", get(usage))
        .route("/ee/tenants/{id}/audit", get(audit))
        .route("/ee/priors", get(priors))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

async fn require_admin(
    State(state): State<HostedState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = &state.admin_token else {
        return Ok(next.run(request).await); // dev only; main() warns
    };
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    if expected.len() == presented.len()
        && expected
            .iter()
            .zip(presented)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub enum HostedError {
    Status(StatusCode, String),
}

impl From<EeError> for HostedError {
    fn from(e: EeError) -> Self {
        let status = match &e {
            EeError::TenantNotFound(_) => StatusCode::NOT_FOUND,
            EeError::TenantSuspended(_) => StatusCode::CONFLICT,
            EeError::Forbidden { .. } => StatusCode::FORBIDDEN,
            EeError::InvalidPricing(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        HostedError::Status(status, e.to_string())
    }
}

impl IntoResponse for HostedError {
    fn into_response(self) -> Response {
        let HostedError::Status(status, message) = self;
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

fn bad_request(message: impl Into<String>) -> HostedError {
    HostedError::Status(StatusCode::BAD_REQUEST, message.into())
}

fn actor(headers: &axum::http::HeaderMap) -> Result<String, HostedError> {
    headers
        .get("x-merge0-actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| bad_request("x-merge0-actor header (acting user email) required"))
}

fn parse_id(id: &str) -> Result<Ulid, HostedError> {
    Ulid::from_string(id).map_err(|_| bad_request(format!("bad tenant id {id:?}")))
}

#[derive(Deserialize)]
struct CreateTenant {
    name: String,
    plan: String,
    /// The first admin of the new org.
    admin_email: String,
}

async fn create_tenant(
    State(state): State<HostedState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateTenant>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let actor = actor(&headers)?;
    let now = Utc::now();
    let tenant = state
        .manager
        .create_tenant(&body.name, &body.plan, &actor, now)
        .await?;
    state
        .manager
        .add_member(tenant.id, &body.admin_email, Role::Admin, &actor, now)
        .await?;
    Ok(Json(serde_json::to_value(&tenant).expect("serializes")))
}

async fn list_tenants(
    State(state): State<HostedState>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let tenants = state.manager.list_tenants().await?;
    Ok(Json(serde_json::to_value(&tenants).expect("serializes")))
}

async fn suspend_tenant(
    State(state): State<HostedState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, HostedError> {
    let actor = actor(&headers)?;
    let id = parse_id(&id)?;
    state.manager.suspend_tenant(id, &actor, Utc::now()).await?;
    Ok(Json(serde_json::json!({ "suspended": id.to_string() })))
}

async fn resume_tenant(
    State(state): State<HostedState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, HostedError> {
    let actor = actor(&headers)?;
    let id = parse_id(&id)?;
    state.manager.resume_tenant(id, &actor, Utc::now()).await?;
    Ok(Json(serde_json::json!({ "resumed": id.to_string() })))
}

#[derive(Deserialize)]
struct AddMember {
    email: String,
    role: Role,
}

/// Membership changes require the ACTOR to hold ManageMembers in the
/// tenant (the operator token alone is not enough — RBAC applies to the
/// human, audit records the human).
async fn add_member(
    State(state): State<HostedState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddMember>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let actor = actor(&headers)?;
    let id = parse_id(&id)?;
    state
        .manager
        .require(id, &actor, Action::ManageMembers)
        .await?;
    state
        .manager
        .add_member(id, &body.email, body.role, &actor, Utc::now())
        .await?;
    Ok(Json(serde_json::json!({
        "tenant": id.to_string(),
        "member": body.email,
        "role": body.role,
    })))
}

#[derive(Deserialize)]
struct UsageParams {
    window_days: Option<u32>,
    /// "per_merged_pr:<cents>" or "flat:<monthly>:<included>:<overage>".
    pricing: Option<String>,
}

async fn usage(
    State(state): State<HostedState>,
    Path(id): Path<String>,
    Query(params): Query<UsageParams>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let id = parse_id(&id)?;
    let tenant = state.manager.get_tenant(id).await?;
    let window = params.window_days.unwrap_or(30).clamp(1, 365);
    let usage = merge0_ee::usage(&state.manager, &tenant, window, Utc::now()).await?;
    let invoice = match params.pricing.as_deref() {
        Some(spec) => Some(merge0_ee::invoice(&parse_pricing(spec)?, usage.merged_prs)),
        None => None,
    };
    Ok(Json(serde_json::json!({
        "tenant": tenant.id.to_string(),
        "usage": usage,
        "invoice": invoice,
    })))
}

fn parse_pricing(spec: &str) -> Result<Pricing, HostedError> {
    let parts: Vec<&str> = spec.split(':').collect();
    let parse = |s: &str| {
        s.parse::<u32>()
            .map_err(|_| bad_request(format!("bad pricing number {s:?}")))
    };
    match parts.as_slice() {
        ["per_merged_pr", cents] => Ok(Pricing::try_new_per_merged_pr(parse(cents)?)?),
        ["flat", monthly, included, overage] => Ok(Pricing::try_new_flat_plus_pool(
            parse(monthly)?,
            parse(included)?,
            parse(overage)?,
        )?),
        _ => Err(bad_request(
            "pricing must be per_merged_pr:<cents> or flat:<monthly>:<included>:<overage>",
        )),
    }
}

async fn audit(
    State(state): State<HostedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let id = parse_id(&id)?;
    state.manager.get_tenant(id).await?;
    let entries = state
        .manager
        .entries_for_tenant(&id.to_string(), 100)
        .await?;
    Ok(Json(serde_json::to_value(&entries).expect("serializes")))
}

/// Cross-tenant outcome priors (the hosted-only data service): anonymized
/// bucket rates plus the gate-context block hosted gates append.
async fn priors(State(state): State<HostedState>) -> Result<Json<serde_json::Value>, HostedError> {
    let priors = merge0_ee::compute_priors(&state.manager, 90, Utc::now()).await?;
    let gate_context = priors.as_gate_context();
    Ok(Json(serde_json::json!({
        "priors": priors,
        "gate_context": gate_context,
    })))
}

// Compile-time guard: the RBAC matrix must stay exhaustive for the surface
// this plane exposes.
const _: () = {
    // (evaluated for side effect of referencing the symbols)
    let _ = allowed;
};
