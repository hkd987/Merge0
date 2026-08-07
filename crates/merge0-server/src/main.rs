//! Service entry point. All configuration is environment-driven; secrets
//! arrive as env values provided by the operator's own infrastructure and
//! are never logged. Fail-fast validation at boot (audit O4): a bad
//! MERGE0_REPO dies here, not when a reviewer clicks Approve.

use merge0_context::intent::IntentDoc;
use merge0_github::api::RestGitHub;
use merge0_github::auth::{AppAuth, InstallationTokenSource};
use merge0_github::RepoRef;
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
    // Fail fast on a malformed repo (owner/name) — audit O4.
    let repo = RepoRef::parse(&required("MERGE0_REPO")?)
        .map_err(|e| format!("MERGE0_REPO invalid: {e}"))?;
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
    let pr_body_template =
        std::fs::read_to_string(std::path::Path::new(&config_dir).join("pr-body-template.md"))
            .map_err(|e| format!("config/pr-body-template.md unreadable: {e}"))?;

    // Fallback intent (the live MERGE0.md is fetched from the customer repo
    // per triage run): a local override path, else the shipped template.
    let intent_text = match std::env::var("MERGE0_INTENT_FALLBACK") {
        Ok(path) => std::fs::read_to_string(path)?,
        Err(_) => merge0_context::intent::MERGE0_TEMPLATE.to_string(),
    };
    let intent_fallback = IntentDoc::parse(&intent_text)
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
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
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
        tracing::warn!("MERGE0_API_TOKEN unset — the API is OPEN (dev only)");
    }

    // Fetch layer (audit C1): pollers built from config/sources.toml; the
    // file ships with everything disabled, so a fresh install runs
    // envelope-only until sources are enabled.
    let sources_path = std::path::Path::new(&config_dir).join("sources.toml");
    let sources = if sources_path.exists() {
        merge0_fetch::config::SourcesConfig::load(&sources_path)
            .map_err(|e| format!("config/sources.toml: {e}"))?
    } else {
        merge0_fetch::config::SourcesConfig::default()
    };
    let fetchers = merge0_fetch::config::build_fetchers(&sources, github.clone())
        .map_err(|e| format!("fetch layer init: {e}"))?;
    if !fetchers.is_empty() {
        tracing::info!(
            "fetch layer: {} source(s) enabled: {}",
            fetchers.len(),
            fetchers
                .iter()
                .map(|f| f.source_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let vendor_webhooks = merge0_server::VendorWebhooks {
        sentry_client_secret: std::env::var("MERGE0_SENTRY_WEBHOOK_SECRET").ok(),
        posthog_shared_token: std::env::var("MERGE0_POSTHOG_WEBHOOK_TOKEN").ok(),
        zendesk_signing_secret: std::env::var("MERGE0_ZENDESK_WEBHOOK_SECRET").ok(),
        datadog_shared_token: std::env::var("MERGE0_DATADOG_WEBHOOK_TOKEN").ok(),
        posthog_project_base_url: sources
            .posthog
            .as_ref()
            .map(|p| p.project_base_url.clone())
            .unwrap_or_else(|| "https://us.posthog.com".into()),
        zendesk_agent_base_url: sources
            .zendesk
            .as_ref()
            .map(|z| z.agent_base_url.clone())
            .unwrap_or_else(|| "https://example.zendesk.com/agent".into()),
        datadog_app_base_url: sources
            .datadog
            .as_ref()
            .map(|d| d.app_base_url.clone())
            .unwrap_or_else(|| "https://app.datadoghq.com".into()),
    };

    let state = AppState {
        tenant,
        model,
        github,
        slack,
        scouts: Arc::new(scouts),
        gate: Arc::new(gate),
        repo,
        intent_fallback: Arc::new(intent_fallback),
        agent: agent_kind_from_env(),
        pr_body_template: Arc::new(pr_body_template),
        callback_url: std::env::var("MERGE0_CALLBACK_URL")
            .unwrap_or_else(|_| "http://localhost:8080/runner/callback".into()),
        inbox_url: std::env::var("MERGE0_PUBLIC_URL")
            .unwrap_or_else(|_| "http://localhost:8080".into()),
        api_token,
        runner_token: std::env::var("MERGE0_RUNNER_TOKEN").ok(),
        webhook_secret: std::env::var("MERGE0_GITHUB_WEBHOOK_SECRET").ok(),
        slack_signing_secret: std::env::var("MERGE0_SLACK_SIGNING_SECRET").ok(),
        hardening_enabled: std::env::var("MERGE0_HARDENING_ENABLED").as_deref() == Ok("1"),
        fetchers: Arc::new(fetchers),
        vendor_webhooks: Arc::new(vendor_webhooks),
    };

    spawn_schedulers(&state);

    // Default bind is loopback (audit O5): exposing the port is an explicit
    // decision (containers set MERGE0_BIND=0.0.0.0:8080).
    let bind = std::env::var("MERGE0_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("merge0-server listening on {bind}");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Interval work: triage runs (nightly default), weekly meta-loop, daily
/// raw-payload retention.
fn spawn_schedulers(state: &AppState) {
    // Cron-driven scout runs (PRD: nightly default, configurable; 0 = off).
    let interval_secs: u64 = std::env::var("MERGE0_TRIAGE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24 * 60 * 60);
    if interval_secs > 0 {
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                // Fetch first: pull fresh vendor signals, then triage them.
                for (source, result) in
                    merge0_fetch::run_all(&state.fetchers, &state.tenant, chrono::Utc::now()).await
                {
                    match result {
                        Ok(outcome) => tracing::info!(?outcome, "fetched {source}"),
                        Err(e) => tracing::error!("fetch {source} failed: {e}"),
                    }
                }
                match merge0_server::handlers::triage::run_once(&state).await {
                    Ok(run) => tracing::info!(?run, "scheduled triage run complete"),
                    Err(e) => tracing::error!("scheduled triage run failed: {e}"),
                }
            }
        });
    }

    // Meta-loop (PRD §5d), weekly: telemetry in as Signals, config-change
    // proposals out as PRs. Flag-gated.
    if std::env::var("MERGE0_META_ENABLED").as_deref() == Ok("1") {
        let state = state.clone();
        tokio::spawn(async move {
            let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
            let mut ticker = tokio::time::interval(week);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = run_meta_loop(&state).await {
                    tracing::error!("meta-loop failed: {e}");
                }
            }
        });
    }

    // Raw-payload retention (audit O7): daily purge when configured.
    if let Some(days) = std::env::var("MERGE0_RAW_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
    {
        let state = state.clone();
        tokio::spawn(async move {
            let day = std::time::Duration::from_secs(24 * 60 * 60);
            let mut ticker = tokio::time::interval(day);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
                match state.tenant.purge_raw_older_than(cutoff).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("retention: redacted raw payloads on {n} signals"),
                    Err(e) => tracing::error!("retention purge failed: {e}"),
                }
            }
        });
    }
}

/// One meta-loop pass (PRD §5d): snapshot telemetry, ingest it as `meta`
/// Signals, and open evidence-linked config-change PRs for any proposals.
async fn run_meta_loop(state: &AppState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use merge0_adapters::Adapter;
    let now = chrono::Utc::now();
    let snapshot = state.tenant.telemetry(30, now).await?;

    let envelope = serde_json::json!({
        "endpoint": "telemetry",
        "context": { "captured_at": now.to_rfc3339() },
        "payload": serde_json::to_value(&snapshot)?,
    });
    for signal in merge0_meta::MetaAdapter.normalize(&envelope)? {
        state.tenant.upsert_signal(&signal).await?;
    }

    let gate_toml = std::fs::read_to_string(
        std::path::Path::new(
            &std::env::var("MERGE0_CONFIG_DIR").unwrap_or_else(|_| "config".into()),
        )
        .join("gate.toml"),
    )?;
    for proposal in merge0_meta::MetaScout::propose_config_changes(&snapshot, &gate_toml) {
        let pr = merge0_meta::open_meta_pr(&proposal, state.github.as_ref(), &state.repo).await?;
        tracing::info!("meta-loop proposed {}", pr.url);
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler installs")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received — draining in-flight requests");
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
