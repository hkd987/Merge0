//! The Merge0 service: ingestion endpoints, triage runs, the inbox (P0-7),
//! runner callbacks (P0-6), GitHub webhooks (P0-8), safety verification
//! (P0-9), the telemetry dashboard (P0-10), onboarding (§6a item 3), Slack
//! interactivity (§6), and the hardening (§5c) / meta-loop (§5d) triggers.
//! Single-org in the MIT core; multi-tenant org management lives in `/ee`.

use merge0_github::{GitHubApi, RepoRef};
use merge0_model::Model;
use merge0_runner::AgentKind;
use merge0_slack::SlackSink;
use merge0_store::TenantStore;
use merge0_triage::config::{GateConfig, ScoutConfig};
use std::sync::Arc;

pub mod handlers;
pub mod intent;
pub mod ratelimit;
pub mod router;

pub use router::app;

/// Everything handlers need. Trait objects for the model, GitHub, and Slack
/// keep the whole surface testable with fakes.
#[derive(Clone)]
pub struct AppState {
    pub tenant: TenantStore,
    pub model: Arc<dyn Model>,
    pub github: Arc<dyn GitHubApi>,
    pub slack: Option<Arc<dyn SlackSink>>,
    pub scouts: Arc<Vec<ScoutConfig>>,
    pub gate: Arc<GateConfig>,
    /// The customer repo work orders target — parsed and validated at boot.
    pub repo: RepoRef,
    /// Fallback intent text used when the customer repo has no MERGE0.md
    /// (the live doc is fetched from the repo per triage run — PRD §3).
    pub intent_fallback: Arc<String>,
    pub agent: AgentKind,
    /// The versioned PR-body template (config/pr-body-template.md).
    pub pr_body_template: Arc<String>,
    /// Where the customer workflow posts its RunReport.
    pub callback_url: String,
    /// Public base URL of this server (deep links in Slack messages).
    pub inbox_url: String,
    /// Bearer token required on the API (None = open, dev only — main()
    /// warns loudly).
    pub api_token: Option<String>,
    /// Bearer token required on the runner callback.
    pub runner_token: Option<String>,
    /// HMAC secret for GitHub webhooks (None = reject all webhooks).
    pub webhook_secret: Option<String>,
    /// Slack request-signing secret for `/slack/interactions` (None =
    /// reject interactions).
    pub slack_signing_secret: Option<String>,
    /// Hardening pass trigger on merged PRs (PRD §5c — ships after the
    /// Phase 0 gate; wiring exists, default off).
    pub hardening_enabled: bool,
    /// Fetch-layer pollers built from `config/sources.toml` (empty when no
    /// source is enabled); the scheduler runs them before each triage pass.
    pub fetchers: Arc<Vec<Box<dyn merge0_fetch::Fetcher>>>,
    /// Native vendor webhook verification + deep-link context.
    pub vendor_webhooks: Arc<VendorWebhooks>,
    /// Per-IP rate limiting on the OPEN routes (None = disabled).
    pub rate_limiter: Option<Arc<ratelimit::RateLimiter>>,
    /// Escalation re-open factor for dismissed reports (`MERGE0_REOPEN_FACTOR`,
    /// default 3; 0 disables re-opens).
    pub reopen_factor: u64,
    /// Days a merged fix must stay quiet before it counts as confirmed
    /// (`MERGE0_EFFICACY_GRACE_DAYS`, default 3).
    pub efficacy_grace_days: u32,
    /// Slack notification classes (`MERGE0_SLACK_NOTIFY`, default both):
    /// new-report pushes and PR-ready pushes, individually killable.
    pub notify_reports: bool,
    pub notify_pr_ready: bool,
    /// Credential broker (PRD §5a, P2): single-use per-Work-Order grants
    /// behind `POST /broker/credentials`. None = surface disabled
    /// (`MERGE0_BROKER_RUNNER_KEY` unset). The shipped minter is the
    /// deterministic preview minter; production Git credentials remain the
    /// runner's own token until the hosted minter lands.
    pub broker: Option<Arc<tokio::sync::Mutex<merge0_broker::Broker<merge0_broker::FakeMinter>>>>,
    /// Curated skill registry surface (PRD §5b, P2): a local signed-index
    /// directory + the pinned verifying key. None = routes 503.
    pub registry: Option<Arc<RegistryHandle>>,
    /// What approval delivers: a PR (default), a tracker story, or both.
    pub delivery_mode: handlers::actions::DeliveryMode,
    /// Files stories for the story-bearing delivery modes. None = story
    /// modes cannot deliver (approval fails loudly rather than silently
    /// doing nothing).
    pub tracker: Option<Arc<dyn merge0_tracker::Tracker>>,
}

/// The registry surface's configuration: where the signed index and skill
/// packages live, and the pinned trust root that signs the index.
pub struct RegistryHandle {
    /// Directory containing `index.json` (a serialized `SignedIndex`) and
    /// `skills/<name>/<files>` package payloads.
    pub dir: std::path::PathBuf,
    pub verifying_key: merge0_registry::VerifyingKey,
}

/// Configuration for `/webhooks/{vendor}` receivers: per-vendor
/// verification material (env-provided) and the deep-link bases the
/// envelope builders need. A vendor with no secret configured is rejected —
/// unauthenticated vendor webhooks are not accepted.
#[derive(Default)]
pub struct VendorWebhooks {
    pub sentry_client_secret: Option<String>,
    pub posthog_shared_token: Option<String>,
    pub zendesk_signing_secret: Option<String>,
    pub datadog_shared_token: Option<String>,
    pub jira_shared_token: Option<String>,
    pub linear_signing_secret: Option<String>,
    /// Slack Events API requests verify with the same app signing secret
    /// as `/slack/interactions`.
    pub slack_signing_secret: Option<String>,
    pub posthog_project_base_url: String,
    pub zendesk_agent_base_url: String,
    pub datadog_app_base_url: String,
    pub jira_browse_base_url: String,
    pub slack_team_base_url: String,
}
