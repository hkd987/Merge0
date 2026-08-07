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

    // MERGE0_DEV_FAKES=1 swaps the model and GitHub for in-process fakes so
    // the complete loop can be driven locally (manual e2e) without a live
    // model or GitHub App. Loudly not for production.
    let dev_fakes = std::env::var("MERGE0_DEV_FAKES").as_deref() == Ok("1");
    let (model, github): (
        Arc<dyn merge0_model::Model>,
        Arc<dyn merge0_github::GitHubApi>,
    ) = if dev_fakes {
        tracing::warn!("MERGE0_DEV_FAKES=1 — model and GitHub are FAKES (dev only)");
        let fake_github = merge0_github::FakeGitHub::new().with_protection(
            merge0_github::api::BranchProtection {
                protected: true,
                required_checks: true,
            },
        );
        let fixed = merge0_model::FixedModel {
            response: r#"{"decision":"work","summary":"Fix the reported defect",
                "repro":"see evidence links","success_criteria":"regression test passes",
                "constraints":"stay within the diff budget"}"#
                .to_string(),
        };
        (Arc::new(fixed), Arc::new(fake_github))
    } else {
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
        (Arc::new(model), Arc::new(github))
    };

    let slack = std::env::var("MERGE0_SLACK_WEBHOOK_URL")
        .ok()
        .map(|url| Arc::new(WebhookSink::new(url)) as Arc<dyn merge0_slack::SlackSink>);

    let api_token = std::env::var("MERGE0_API_TOKEN").ok();
    if api_token.is_none() {
        tracing::warn!("MERGE0_API_TOKEN unset — mutating endpoints are OPEN (dev only)");
    }

    let state = AppState {
        tenant,
        model,
        github,
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
    // Cron-driven scout runs (PRD: nightly default, configurable).
    let interval_secs: u64 = std::env::var("MERGE0_TRIAGE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24 * 60 * 60);
    if interval_secs > 0 {
        let scheduler_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                match merge0_triage::pipeline::run_triage(
                    &scheduler_state.tenant,
                    scheduler_state.model.as_ref(),
                    &scheduler_state.scouts,
                    &scheduler_state.gate,
                    &scheduler_state.intent_text,
                    &scheduler_state.repo,
                    chrono::Utc::now(),
                )
                .await
                {
                    Ok(run) => tracing::info!(?run, "scheduled triage run complete"),
                    Err(e) => tracing::error!("scheduled triage run failed: {e}"),
                }
            }
        });
    }

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
