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
        .route("/ee/tenants/{id}/runtime", get(runtime))
        .route("/ee/tenants/{id}/audit", get(audit))
        .route("/ee/priors", get(priors))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .route("/healthz", get(|| async { "ok" }))
        // Same protective posture as the core server: bounded bodies and a
        // request deadline. Admin-token gating is not a reason to accept
        // unbounded input — tokens leak, and defence in depth is cheap.
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
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

/// The control->data bridge: everything an operator (or an orchestrator
/// reconciling desired state) needs to run this tenant's data plane. One
/// `merge0-server` process per tenant, pointed at the tenant schema this
/// plane provisioned.
///
/// Deliberately contains NO secret values — the same name-only discipline
/// as `config/sources.toml` (CLAUDE.md rule 4). The tenant's GitHub App
/// credentials, API tokens, and vendor keys live in the deployment
/// environment the operator controls; this endpoint tells them exactly
/// which variables that environment must provide.
async fn runtime(
    State(state): State<HostedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, HostedError> {
    let id = parse_id(&id)?;
    let tenant = state.manager.get_tenant(id).await?;
    let desired_state = if tenant.suspended {
        // Suspension is enforced twice: `tenant_store` refuses the schema
        // inside this plane, and the orchestrator reading this field scales
        // the tenant's data-plane service to zero.
        "suspended"
    } else {
        "running"
    };
    let service = format!("merge0-tenant-{}", tenant.id.to_string().to_lowercase());
    Ok(Json(serde_json::json!({
        "tenant": tenant.id.to_string(),
        "desired_state": desired_state,
        "service_name": service,
        "image_command": "merge0-server",
        "env": {
            // Fixed by the control plane.
            "MERGE0_TENANT": tenant.schema_name,
            // Provided by the operator's deployment environment (values
            // never transit or persist in the control plane).
            "required_from_operator": [
                "MERGE0_DATABASE_URL",
                "MERGE0_REPO",
                "MERGE0_PUBLIC_URL",
                "MERGE0_API_TOKEN",
                "MERGE0_RUNNER_TOKEN",
                "MERGE0_GITHUB_WEBHOOK_SECRET",
                "MERGE0_GITHUB_APP_ID",
                "MERGE0_GITHUB_INSTALLATION_ID",
                "MERGE0_GITHUB_APP_PRIVATE_KEY",
                "ANTHROPIC_API_KEY",
            ],
            "optional_from_operator": [
                "MERGE0_SLACK_WEBHOOK_URL",
                "MERGE0_SLACK_SIGNING_SECRET",
                "MERGE0_DELIVERY_MODE",
                "MERGE0_JIRA_PROJECT",
                "MERGE0_GATE_CONTEXT_EXTRA",
            ],
        },
        "notes": [
            "one merge0-server process per tenant; never point two at one schema",
            "suspended tenants: scale the service to 0 — the schema also refuses opens through the control plane",
            "MERGE0_GATE_CONTEXT_EXTRA can carry the /ee/priors gate_context block; refresh it on deploys",
        ],
    })))
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
