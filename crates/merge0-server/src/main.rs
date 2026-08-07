//! Service entry point. All configuration is environment-driven; secrets
//! arrive as env values provided by the operator's own infrastructure and
//! are never logged.

use merge0_context::intent::IntentDoc;
use merge0_github::api::RestGitHub;
use merge0_github::auth::{AppAuth, InstallationTokenSource};
use merge0_model::AnthropicModel;
use merge0_runner::AgentKind;
use merge0_server::{app, AppState};
use merge0_slack::WebhookSink;
use merge0_store::Store;
use merge0_triage::config as triage_config;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = required("MERGE0_DATABASE_URL")?;
    let tenant_schema = std::env::var("MERGE0_TENANT").unwrap_or_else(|_| "tenant_default".into());
    let repo = required("MERGE0_REPO")?;
    let config_dir = std::env::var("MERGE0_CONFIG_DIR").unwrap_or_else(|_| "config".into());

    let store = Store::connect(&database_url).await?;
    let tenant = store.tenant(&tenant_schema).await?;

    let scouts =
        triage_config::load_scouts(std::path::Path::new(&config_dir).join("scouts").as_path())?;
    let gate = triage_config::load_gate(
        std::path::Path::new(&config_dir)
            .join("gate.toml")
            .as_path(),
    )?;

    // Intent doc: local path override, else the shipped template.
    let intent_text = match std::env::var("MERGE0_INTENT_DOC") {
        Ok(path) => std::fs::read_to_string(path)?,
        Err(_) => merge0_context::intent::MERGE0_TEMPLATE.to_string(),
    };
    let intent_human = IntentDoc::parse(&intent_text)
        .map(|doc| doc.human_text())
        .unwrap_or(intent_text);

    // Model: the customer's own key (BYO), never logged.
    let model = AnthropicModel::new(
        required("ANTHROPIC_API_KEY")?,
        std::env::var("MERGE0_GATE_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into()),
    );

    // GitHub App auth (P0-11): installation tokens only.
    let github = RestGitHub::new(InstallationTokenSource {
        auth: AppAuth::new(
            required("MERGE0_GITHUB_APP_ID")?,
            required("MERGE0_GITHUB_APP_PRIVATE_KEY")?,
        ),
        client: reqwest_client(),
        base_url: "https://api.github.com".into(),
        installation_id: required("MERGE0_GITHUB_INSTALLATION_ID")?.parse()?,
    });

    let slack = std::env::var("MERGE0_SLACK_WEBHOOK_URL")
        .ok()
        .map(|url| Arc::new(WebhookSink::new(url)) as Arc<dyn merge0_slack::SlackSink>);

    let api_token = std::env::var("MERGE0_API_TOKEN").ok();
    if api_token.is_none() {
        tracing::warn!("MERGE0_API_TOKEN unset — mutating endpoints are OPEN (dev only)");
    }

    let state = AppState {
        tenant,
        model: Arc::new(model),
        github: Arc::new(github),
        slack,
        scouts: Arc::new(scouts),
        gate: Arc::new(gate),
        repo,
        intent_text: Arc::new(intent_human),
        agent: agent_kind_from_env(),
        callback_url: std::env::var("MERGE0_CALLBACK_URL")
            .unwrap_or_else(|_| "http://localhost:8080/runner/callback".into()),
        inbox_url: std::env::var("MERGE0_PUBLIC_URL")
            .unwrap_or_else(|_| "http://localhost:8080".into()),
        api_token,
        runner_token: std::env::var("MERGE0_RUNNER_TOKEN").ok(),
        webhook_secret: std::env::var("MERGE0_GITHUB_WEBHOOK_SECRET").ok(),
    };

    let bind = std::env::var("MERGE0_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("merge0-server listening on {bind}");
    axum::serve(listener, app(state)).await?;
    Ok(())
}

fn required(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing required env var {name}"))
}

fn agent_kind_from_env() -> AgentKind {
    match std::env::var("MERGE0_AGENT").as_deref() {
        Ok("codex-cli") => AgentKind::CodexCli,
        Ok(custom) if custom.starts_with("custom:") => {
            AgentKind::Custom(custom.trim_start_matches("custom:").to_string())
        }
        _ => AgentKind::ClaudeCode,
    }
}

fn reqwest_client() -> reqwest::Client {
    reqwest::Client::new()
}
