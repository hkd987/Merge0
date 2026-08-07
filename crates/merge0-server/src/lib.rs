//! The Merge0 service: ingestion endpoints, triage runs, the inbox (P0-7),
//! runner callbacks (P0-6), GitHub webhooks (P0-8), safety verification
//! (P0-9), and the telemetry dashboard (P0-10). Single-org in the MIT core;
//! multi-tenant org management lives in `/ee`.

use merge0_github::GitHubApi;
use merge0_model::Model;
use merge0_runner::AgentKind;
use merge0_slack::SlackSink;
use merge0_store::TenantStore;
use merge0_triage::config::{GateConfig, ScoutConfig};
use std::sync::Arc;

pub mod handlers;
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
    /// The customer repo work orders target, `owner/name`.
    pub repo: String,
    /// Customer intent doc (MERGE0.md) human prose, fence-stripped.
    pub intent_text: Arc<String>,
    pub agent: AgentKind,
    /// Where the customer workflow posts its RunReport.
    pub callback_url: String,
    /// Public base URL of this server (deep links in Slack messages).
    pub inbox_url: String,
    /// Bearer token required on mutating API calls (None = open, dev only).
    pub api_token: Option<String>,
    /// Bearer token required on the runner callback.
    pub runner_token: Option<String>,
    /// HMAC secret for GitHub webhooks (None = reject all webhooks).
    pub webhook_secret: Option<String>,
}
