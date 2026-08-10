//! Service entry point. All configuration is environment-driven; secrets
//! arrive as env values provided by the operator's own infrastructure and
//! are never logged. Fail-fast validation at boot (audit O4): a bad
//! MERGE0_REPO dies here, not when a reviewer clicks Approve.

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
    // MERGE0_GATE_CONTEXT_EXTRA: operator-supplied background context
    // appended to the gate prompt (hosted deployments wire the ee control
    // plane's cross-tenant priors block through this; self-hosters can carry
    // site conventions). Unset or blank is a no-op.
    let gate = triage_config::load_gate(
        std::path::Path::new(&config_dir)
            .join("gate.toml")
            .as_path(),
    )?
    .with_extra_context(std::env::var("MERGE0_GATE_CONTEXT_EXTRA").ok().as_deref());
    let pr_body_template =
        std::fs::read_to_string(std::path::Path::new(&config_dir).join("pr-body-template.md"))
            .map_err(|e| format!("config/pr-body-template.md unreadable: {e}"))?;

    // Fallback intent (the live MERGE0.md is fetched from the customer repo
    // per triage run): a local override path, else the shipped template.
    // Kept whole — the machine fence carries earned constraints, and the
    // gate's own retrieval decides what fits (see server::intent).
    let intent_fallback = match std::env::var("MERGE0_INTENT_FALLBACK") {
        Ok(path) => std::fs::read_to_string(path)?,
        Err(_) => merge0_context::intent::MERGE0_TEMPLATE.to_string(),
    };

    // MERGE0_GATE_BACKEND selects what powers the triage gate:
    //   "api" (default) — AnthropicModel with ANTHROPIC_API_KEY (BYO key).
    //   "claude-cli"    — the Claude Code CLI in print mode, riding whatever
    //     auth the CLI already holds: a Pro/Max/Team subscription login on
    //     the host, or CLAUDE_CODE_OAUTH_TOKEN from `claude setup-token`.
    //     No API key required — the same judgment, billed to the
    //     subscription. Binary overridable via MERGE0_GATE_CLI (tests and
    //     the manual e2e point it at a stub).
    let cli_gate: Option<Arc<dyn merge0_model::Model>> = match std::env::var("MERGE0_GATE_BACKEND")
        .as_deref()
    {
        Err(_) | Ok("api") => None,
        Ok("claude-cli") => {
            let binary = std::env::var("MERGE0_GATE_CLI").unwrap_or_else(|_| "claude".into());
            tracing::info!(
                "gate backend: {binary} CLI (subscription auth — no ANTHROPIC_API_KEY needed)"
            );
            Some(Arc::new(
                merge0_model::CliModel::with_binary(binary)
                    .model(std::env::var("MERGE0_GATE_MODEL").ok()),
            ))
        }
        Ok(other) => {
            return Err(format!("MERGE0_GATE_BACKEND invalid: {other:?} (api|claude-cli)").into())
        }
    };

    // MERGE0_DEV_FAKES=1 swaps the model and GitHub for in-process fakes so
    // the complete loop can be driven locally (manual e2e) without a live
    // model or GitHub App. Loudly not for production. A cli gate backend
    // composes with it (fake GitHub, real CLI judgment) — exactly what
    // "try the subscription gate without creating a GitHub App" needs.
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
        // A CODEOWNERS in the fake repo so owner routing is drivable in the
        // manual e2e (report evidence mentioning src/districts/ paths routes
        // to the data team).
        fake_github.state.lock().unwrap().files.insert(
            ".github/CODEOWNERS".into(),
            "* @acme/platform\nsrc/districts/ @acme/data-team\n".into(),
        );
        // PR #424242 reads as already merged, so the outcome-reconciliation
        // sweep (missed-webhook repair) is drivable in the manual e2e.
        fake_github.state.lock().unwrap().pr_states.insert(
            424242,
            merge0_github::PullState {
                state: "closed".into(),
                merged: true,
                merged_at: Some(chrono::Utc::now()),
                closed_at: Some(chrono::Utc::now()),
                merge_commit_sha: Some("fadedfacade0000".into()),
            },
        );
        // Confidence is overridable so the confidence-routing path is
        // drivable end to end (dev fakes only — production reads the real
        // model's own self-assessment and nothing can override it).
        let fake_confidence =
            std::env::var("MERGE0_DEV_FAKE_CONFIDENCE").unwrap_or_else(|_| "high".into());
        let fixed = merge0_model::FixedModel {
            response: format!(
                r#"{{"decision":"work","summary":"Fix the reported defect",
                "repro":"see evidence links","success_criteria":"regression test passes",
                "constraints":"stay within the diff budget","confidence":"{fake_confidence}"}}"#
            ),
        };
        let model: Arc<dyn merge0_model::Model> = match cli_gate {
            Some(cli) => cli,
            None => Arc::new(fixed),
        };
        (model, Arc::new(fake_github))
    } else {
        // Model: the CLI gate when selected, else the customer's own key
        // (BYO), never logged. The API key is only required when it is
        // actually the thing being used.
        let model: Arc<dyn merge0_model::Model> = match cli_gate {
            Some(cli) => cli,
            None => Arc::new(AnthropicModel::new(
                required("ANTHROPIC_API_KEY")?,
                std::env::var("MERGE0_GATE_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into()),
            )),
        };
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
        (model, Arc::new(github))
    };

    let slack = std::env::var("MERGE0_SLACK_WEBHOOK_URL")
        .ok()
        .map(|url| Arc::new(WebhookSink::new(url)) as Arc<dyn merge0_slack::SlackSink>);

    // Tracker (story delivery). Jira reuses the ingestion credentials, so a
    // team already polling Jira only adds a project key. Under dev fakes the
    // recording tracker stands in, so the story path is drivable in e2e
    // without a live Jira.
    let tracker: Option<Arc<dyn merge0_tracker::Tracker>> = if dev_fakes {
        Some(Arc::new(merge0_tracker::RecordingTracker::new()))
    } else {
        match std::env::var("MERGE0_JIRA_PROJECT") {
            Ok(project) => {
                let base_url = required("MERGE0_JIRA_BASE_URL")?;
                let email = required("MERGE0_JIRA_EMAIL")?;
                let token = required("MERGE0_JIRA_API_TOKEN")?;
                let issue_type =
                    std::env::var("MERGE0_JIRA_ISSUE_TYPE").unwrap_or_else(|_| "Task".into());
                Some(Arc::new(
                    merge0_tracker::JiraTracker::new(base_url, email, token, project, issue_type)
                        .map_err(|e| format!("jira tracker init: {e}"))?,
                ))
            }
            Err(_) => None,
        }
    };

    // Confidence routing needs somewhere to route to. Saying so at boot
    // beats a config knob that quietly does nothing for months.
    if tracker.is_none() && gate.delivery.min_confidence_for_pr > merge0_signal::GateConfidence::Low
    {
        tracing::warn!(
            "config/gate.toml sets [delivery] min_confidence_for_pr = {:?} but no tracker is \
             configured (MERGE0_JIRA_PROJECT) — confidence routing is INERT and low-confidence \
             Work Orders will dispatch as PRs",
            gate.delivery.min_confidence_for_pr.as_str()
        );
    }

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
        jira_shared_token: std::env::var("MERGE0_JIRA_WEBHOOK_TOKEN").ok(),
        linear_signing_secret: std::env::var("MERGE0_LINEAR_WEBHOOK_SECRET").ok(),
        // Slack Events verify with the app's signing secret (same as
        // /slack/interactions).
        slack_signing_secret: std::env::var("MERGE0_SLACK_SIGNING_SECRET").ok(),
        jira_browse_base_url: sources
            .jira
            .as_ref()
            .map(|j| format!("{}/browse", j.base_url.trim_end_matches('/')))
            .unwrap_or_else(|| "https://example.atlassian.net/browse".into()),
        slack_team_base_url: sources
            .slack_channels
            .as_ref()
            .map(|s| s.team_base_url.clone())
            .unwrap_or_else(|| "https://example.slack.com".into()),
    };

    let state =
        AppState {
            tenant,
            model,
            github,
            slack,
            scouts: Arc::new(scouts),
            gate: Arc::new(gate),
            repo,
            intent_fallback: Arc::new(intent_fallback),
            agent: agent_kind_from_env()?,
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
            fetch_failures: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            vendor_webhooks: Arc::new(vendor_webhooks),
            // Open-route flood control: default 10 req/s per IP (burst 30);
            // MERGE0_RATE_LIMIT_PER_SECOND=0 disables.
            rate_limiter: merge0_server::ratelimit::RateLimiter::from_rate(
                std::env::var("MERGE0_RATE_LIMIT_PER_SECOND")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            )
            .map(Arc::new),
            reopen_factor: std::env::var("MERGE0_REOPEN_FACTOR")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            efficacy_grace_days: std::env::var("MERGE0_EFFICACY_GRACE_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            notify_reports: slack_notify_enabled("reports"),
            notify_pr_ready: slack_notify_enabled("pr_ready"),
            // Credential broker surface (PRD §5a P2): enabled by registering a
            // per-tenant runner key. Ships with the deterministic preview
            // minter — production Git credentials remain the runner's own token.
            broker: std::env::var("MERGE0_BROKER_RUNNER_KEY").ok().map(|key| {
                let mut broker = merge0_broker::Broker::new(merge0_broker::FakeMinter);
                broker.add_runner_key(key);
                Arc::new(tokio::sync::Mutex::new(broker))
            }),
            // Registry surface (PRD §5b P2): a local signed-index directory
            // plus the pinned hex-encoded verifying key.
            registry: match (
                std::env::var("MERGE0_REGISTRY_DIR").ok(),
                std::env::var("MERGE0_REGISTRY_PUBKEY").ok(),
            ) {
                (Some(dir), Some(pubkey)) => Some(Arc::new(merge0_server::RegistryHandle {
                    dir: dir.into(),
                    verifying_key: merge0_registry::verifying_key_from_hex(&pubkey)
                        .map_err(|e| format!("MERGE0_REGISTRY_PUBKEY invalid: {e}"))?,
                })),
                (Some(_), None) | (None, Some(_)) => {
                    return Err("registry needs BOTH MERGE0_REGISTRY_DIR and \
                     MERGE0_REGISTRY_PUBKEY"
                        .into());
                }
                (None, None) => None,
            },
            // Delivery mode (PRD: a story is the on-ramp for teams not yet
            // ready for autonomous PRs). Default `pr` keeps every existing
            // install behaving identically.
            delivery_mode: match std::env::var("MERGE0_DELIVERY_MODE") {
                Ok(raw) => merge0_server::handlers::actions::DeliveryMode::parse(&raw).ok_or_else(
                    || format!("MERGE0_DELIVERY_MODE invalid: {raw:?} (pr|story|story_and_pr)"),
                )?,
                Err(_) => merge0_server::handlers::actions::DeliveryMode::default(),
            },
            tracker,
        };

    // Release-timeline backfill (PRD P0-4): the webhook only sees releases
    // published AFTER install, so a fresh install has no timeline for
    // first-bad-release attribution until we seed it from the GitHub
    // Releases API once. Best-effort: an API failure logs and moves on —
    // the webhook keeps the timeline current either way.
    match state.tenant.releases().await {
        Ok(existing) if existing.is_empty() => {
            match state.github.list_releases(&state.repo).await {
                Ok(releases) if !releases.is_empty() => {
                    let count = releases.len();
                    for release in releases {
                        state
                            .tenant
                            .upsert_release(
                                &release.tag,
                                release.sha.as_deref(),
                                release.published_at,
                                release.notes.as_deref(),
                            )
                            .await?;
                    }
                    tracing::info!("release timeline backfilled: {count} release(s)");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("release backfill skipped: {e}"),
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("release backfill check failed: {e}"),
    }

    spawn_schedulers(&state);

    // Default bind is loopback (audit O5): exposing the port is an explicit
    // decision (containers set MERGE0_BIND=0.0.0.0:8080).
    let bind = std::env::var("MERGE0_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("merge0-server listening on {bind}");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
                        Err(e) => {
                            tracing::error!("fetch {source} failed: {e}");
                            // Guard dropped at arm end, before the loop's
                            // next await (clippy: await_holding_lock).
                            let mut failures = state.fetch_failures.lock().expect("not poisoned");
                            *failures.entry(source.clone()).or_insert(0) += 1;
                        }
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
    let snapshot = state
        .tenant
        .telemetry(30, state.efficacy_grace_days, now)
        .await?;

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
    std::env::var(name).map_err(|_| {
        // Boot failures are the first thing a new operator sees — point at
        // the fix, not just the symptom.
        let hint = match name {
            "MERGE0_GITHUB_APP_ID"
            | "MERGE0_GITHUB_APP_PRIVATE_KEY"
            | "MERGE0_GITHUB_INSTALLATION_ID" => {
                " — create the GitHub App with docs/github-app-setup.md, \
                 or set MERGE0_DEV_FAKES=1 to run without GitHub"
            }
            "ANTHROPIC_API_KEY" => " — or set MERGE0_DEV_FAKES=1 to run without a model",
            _ => "",
        };
        format!("missing required env var {name}{hint}")
    })
}

/// `MERGE0_SLACK_NOTIFY`: comma-separated notification classes to enable
/// (`reports`, `pr_ready`). Unset = all classes on (the webhook URL itself
/// is the master switch).
fn slack_notify_enabled(class: &str) -> bool {
    match std::env::var("MERGE0_SLACK_NOTIFY") {
        Ok(list) => list.split(',').any(|c| c.trim() == class),
        Err(_) => true,
    }
}

fn agent_kind_from_env() -> Result<AgentKind, String> {
    AgentKind::from_env_value(std::env::var("MERGE0_AGENT").ok().as_deref())
}
