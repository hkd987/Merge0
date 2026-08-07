//! `GET /onboarding` — the product surface for the 15-minute onboarding
//! story (PRD §6a item 3, audit finding M6): everything a repo needs, in
//! one authenticated response — the generated Actions workflow (built from
//! the repo's live manifest), the MERGE0.md and agent.toml templates, the
//! setup checklist, and the current safety verification.

use super::{actions, ApiError};
use crate::AppState;
use axum::extract::State;
use axum::Json;
use merge0_github::safety::verify_repo_safety;

pub async fn bundle(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // Workflow generated against the repo's manifest as it exists right now
    // (defaults when no manifest yet — the checklist tells them to add one).
    let manifest = actions::fetch_manifest(&state).await?;
    let workflow =
        merge0_runner::workflow::workflow_yaml(&state.agent, &manifest, &state.pr_body_template);
    let safety = verify_repo_safety(state.github.as_ref(), &state.repo)
        .await
        .map_err(ApiError::internal)?;
    let safety_ok = safety.satisfied();
    let safety_failures = safety.failures();

    Ok(Json(serde_json::json!({
        "repo": state.repo.full(),
        "files": {
            ".github/workflows/merge0.yml": workflow,
            "MERGE0.md": merge0_context::intent::MERGE0_TEMPLATE,
            ".merge0/agent.toml": merge0_runner::manifest::AGENT_TOML_TEMPLATE,
        },
        "secrets_to_configure": {
            "ANTHROPIC_API_KEY": "your Anthropic key — the agent runs on YOUR account",
            "MERGE0_RUNNER_TOKEN": "must match this server's MERGE0_RUNNER_TOKEN",
        },
        "checklist": [
            "Commit the three files above via a normal PR (review them — they run in YOUR CI)",
            "Add the two Actions secrets listed in secrets_to_configure",
            "Set [test] command in .merge0/agent.toml to your real test entrypoint",
            "Enable branch protection + required status checks on the default branch (Merge0 refuses to dispatch until verified)",
            "Connect vendor credentials for the fetch layer (see config/sources.toml on the server)",
            "Write real invariants into MERGE0.md — the gate reads it on every run",
        ],
        "safety": {
            "satisfied": safety_ok,
            "failures": safety_failures,
            "report": safety,
        },
    })))
}
