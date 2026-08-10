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
        "secrets_to_configure": secrets_to_configure(&state),
        "checklist": [
            "Commit the three files above via a normal PR (review them — they run in YOUR CI)",
            "Add the Actions secrets listed in secrets_to_configure",
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

/// The Actions secrets this repo must configure: the selected agent's own
/// provider key names (BYO agent = BYO account) plus the runner callback
/// token. Names only — values never transit Merge0.
fn secrets_to_configure(state: &AppState) -> serde_json::Value {
    let agent_label = state.agent.label();
    let mut secrets = serde_json::Map::new();
    for name in state.agent.auth_env_names() {
        // The subscription credentials are alternatives, not additions —
        // say so, or every reader assumes all listed secrets are required.
        let hint = match *name {
            "CLAUDE_CODE_OAUTH_TOKEN" => "ALTERNATIVE to ANTHROPIC_API_KEY: Claude Pro/Max/Team subscription token from `claude setup-token` — set one of the two".to_string(),
            "CODEX_AUTH_JSON" => "ALTERNATIVE to OPENAI_API_KEY: contents of ~/.codex/auth.json after a ChatGPT-subscription `codex login` — set one of the two".to_string(),
            _ => format!(
                "model-provider key for the {agent_label} agent — runs on YOUR account (leave unset if this provider is unused)"
            ),
        };
        secrets.insert((*name).to_string(), serde_json::json!(hint));
    }
    secrets.insert(
        "MERGE0_RUNNER_TOKEN".into(),
        serde_json::json!("must match this server's MERGE0_RUNNER_TOKEN"),
    );
    serde_json::Value::Object(secrets)
}
