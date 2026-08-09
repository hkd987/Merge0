//! Typed `config/sources.toml` (PRD §1): which vendors the fetch layer polls
//! and how.
//!
//! Versioned config, not code — enabling a source or changing a base URL is
//! an ordinary reviewed PR, same convention as `config/gate.toml`. Secrets
//! are referenced by env-var **name** only (`*_env` fields, CLAUDE.md rule
//! 4); values are resolved via `std::env::var` at fetcher construction and
//! held in the redacting [`Secret`] newtype — never stored in config, never
//! printed.

use crate::pollers::{
    AsanaPoller, DatadogPoller, GithubIssuesPoller, IntercomPoller, JiraPoller, LinearPoller,
    MixpanelPoller, OpenpanelPoller, PosthogPoller, SentryPoller, SlackChannelsPoller,
    TrelloPoller, ZendeskPoller,
};
use crate::{FetchError, Fetcher};
use merge0_github::GitHubApi;
use serde::Deserialize;
use std::sync::Arc;

/// The whole `config/sources.toml`. Absent sections mean "source not
/// configured"; present-but-disabled sections keep their settings reviewable
/// while excluded from [`build_fetchers`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfig {
    pub posthog: Option<PosthogConfig>,
    pub sentry: Option<SentryConfig>,
    pub zendesk: Option<ZendeskConfig>,
    pub datadog: Option<DatadogConfig>,
    pub github_issues: Option<GithubIssuesConfig>,
    pub jira: Option<JiraConfig>,
    pub linear: Option<LinearConfig>,
    pub slack_channels: Option<SlackChannelsConfig>,
    pub asana: Option<AsanaConfig>,
    pub trello: Option<TrelloConfig>,
    pub intercom: Option<IntercomConfig>,
    pub mixpanel: Option<MixpanelConfig>,
    pub openpanel: Option<OpenpanelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixpanelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Numeric Mixpanel project id (required by the Query API when
    /// authenticating with a service account).
    pub project_id: String,
    /// Env var *names* holding the service-account username and secret
    /// (HTTP Basic auth on the Query API).
    pub service_account_user_env: String,
    pub service_account_secret_env: String,
    /// Query API host; EU projects use https://eu.mixpanel.com, India
    /// https://in.mixpanel.com.
    #[serde(default = "MixpanelConfig::default_base_url")]
    pub base_url: String,
    /// Project UI base for deep links (envelope context `project_base_url`).
    pub project_base_url: String,
    /// Saved funnels polled for drop-off analysis. The funnels query is
    /// Mixpanel's only signal source here, so an enabled section with an
    /// empty list fails at startup rather than running dead.
    #[serde(default)]
    pub funnel_ids: Vec<u64>,
    /// Trailing window re-read each round (funnel results are aggregates,
    /// not events — there is no incremental cursor).
    #[serde(default = "default_lookback_days")]
    pub lookback_days: i64,
}

impl MixpanelConfig {
    fn default_base_url() -> String {
        "https://mixpanel.com".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenpanelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// OpenPanel project id (the export API scopes by project).
    pub project_id: String,
    /// Env var *names* holding a **read**-mode client's id and secret (the
    /// default write client cannot use the export API).
    pub client_id_env: String,
    pub client_secret_env: String,
    /// API host; self-hosted deployments override this.
    #[serde(default = "OpenpanelConfig::default_base_url")]
    pub base_url: String,
    /// Dashboard base for deep links (envelope context `project_base_url`).
    pub project_base_url: String,
    /// Which event names are defect signals (OpenPanel has no built-in
    /// error tracking, so this is an operator decision). An enabled section
    /// with an empty list fails at startup rather than running dead.
    #[serde(default)]
    pub error_events: Vec<String>,
    /// First-round trailing window; later rounds are incremental via the
    /// stored cursor.
    #[serde(default = "default_lookback_days")]
    pub lookback_days: i64,
}

impl OpenpanelConfig {
    fn default_base_url() -> String {
        "https://api.openpanel.dev".into()
    }
}

fn default_lookback_days() -> i64 {
    7
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PosthogConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// PostHog project id (path segment of the API routes).
    pub project_id: String,
    /// Env var *name* holding a personal API key with error-tracking read.
    pub api_key_env: String,
    #[serde(default = "PosthogConfig::default_base_url")]
    pub base_url: String,
    /// Project UI base for deep links (envelope context `project_base_url`).
    pub project_base_url: String,
    /// Funnel insights (numeric ids or short ids) polled for drop-off
    /// analysis. Empty (the default) skips the funnels endpoint entirely.
    #[serde(default)]
    pub funnel_insight_ids: Vec<String>,
}

impl PosthogConfig {
    fn default_base_url() -> String {
        "https://us.posthog.com".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub organization: String,
    pub project: String,
    /// Env var *name* holding an auth token with `event:read`.
    pub auth_token_env: String,
    #[serde(default = "SentryConfig::default_base_url")]
    pub base_url: String,
}

impl SentryConfig {
    fn default_base_url() -> String {
        "https://sentry.io".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZendeskConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `{subdomain}.zendesk.com` — the API base is derived from it unless
    /// `base_url` overrides (tests point it at a mock server).
    pub subdomain: String,
    /// Env var *name* holding the agent email the API token belongs to.
    pub email_env: String,
    /// Env var *name* holding the API token.
    pub api_token_env: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Agent-workspace base for deep links (envelope context
    /// `agent_base_url`).
    pub agent_base_url: String,
}

impl ZendeskConfig {
    pub fn effective_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| format!("https://{}.zendesk.com", self.subdomain))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatadogConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *name* holding the API key (`DD-API-KEY`).
    pub api_key_env: String,
    /// Env var *name* holding the application key (`DD-APPLICATION-KEY`).
    pub app_key_env: String,
    #[serde(default = "DatadogConfig::default_base_url")]
    pub base_url: String,
    /// Datadog app base for deep links (envelope context `app_base_url`).
    pub app_base_url: String,
}

impl DatadogConfig {
    fn default_base_url() -> String {
        "https://api.datadoghq.com".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubIssuesConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `owner/name`. Auth rides on the shared [`GitHubApi`] handle — no
    /// separate secret here.
    pub repo: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JiraConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Jira Cloud site base; browse links derive from it.
    pub base_url: String,
    /// Env var *names*: Jira Cloud API auth is basic auth (email + token).
    pub email_env: String,
    pub api_token_env: String,
    /// JQL the poller runs; the cursor ANDs an `updated >=` bound onto it.
    #[serde(default = "JiraConfig::default_jql")]
    pub jql: String,
}

impl JiraConfig {
    fn default_jql() -> String {
        "statusCategory != Done ORDER BY updated ASC".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *name* holding a Linear API key.
    pub api_key_env: String,
    #[serde(default = "LinearConfig::default_base_url")]
    pub base_url: String,
}

impl LinearConfig {
    fn default_base_url() -> String {
        "https://api.linear.app".into()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackChannel {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackChannelsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *name* holding a bot token with `channels:history`.
    pub bot_token_env: String,
    #[serde(default = "SlackChannelsConfig::default_base_url")]
    pub base_url: String,
    /// Workspace base for archive permalinks (envelope context).
    pub team_base_url: String,
    /// Channels treated as ticket streams by team convention.
    pub channels: Vec<SlackChannel>,
}

impl SlackChannelsConfig {
    fn default_base_url() -> String {
        "https://slack.com/api".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsanaConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *name* holding a personal access token.
    pub pat_env: String,
    #[serde(default = "AsanaConfig::default_base_url")]
    pub base_url: String,
    /// Projects whose tasks are polled.
    pub project_gids: Vec<String>,
}

impl AsanaConfig {
    fn default_base_url() -> String {
        "https://app.asana.com/api/1.0".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrelloConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *names* for the key+token query-auth pair.
    pub key_env: String,
    pub token_env: String,
    #[serde(default = "TrelloConfig::default_base_url")]
    pub base_url: String,
    /// Boards whose open cards are polled.
    pub board_ids: Vec<String>,
}

impl TrelloConfig {
    fn default_base_url() -> String {
        "https://api.trello.com/1".into()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntercomConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Env var *name* holding an access token.
    pub access_token_env: String,
    #[serde(default = "IntercomConfig::default_base_url")]
    pub base_url: String,
    /// Inbox base for conversation deep links (envelope context).
    pub app_base_url: String,
}

impl IntercomConfig {
    fn default_base_url() -> String {
        "https://api.intercom.io".into()
    }
}

impl SourcesConfig {
    /// Parse a TOML string. Unknown fields anywhere are rejected — a typo'd
    /// key must fail loudly, not silently disable a source.
    pub fn parse(toml_str: &str) -> Result<Self, FetchError> {
        toml::from_str(toml_str).map_err(|e| FetchError::Config(format!("sources.toml: {e}")))
    }

    /// Load from a path (the deployment's `config/sources.toml`).
    pub fn load(path: &std::path::Path) -> Result<Self, FetchError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| FetchError::Config(format!("read {}: {e}", path.display())))?;
        Self::parse(&text)
    }
}

/// Build one [`Fetcher`] per enabled section, resolving secret env vars now
/// so a missing variable fails at startup (naming the variable) instead of
/// mid-poll. The GitHub Issues poller reuses the shared [`GitHubApi`] handle
/// rather than raw HTTP.
pub fn build_fetchers(
    config: &SourcesConfig,
    github: Arc<dyn GitHubApi>,
) -> Result<Vec<Box<dyn Fetcher>>, FetchError> {
    let mut fetchers: Vec<Box<dyn Fetcher>> = Vec::new();
    if let Some(c) = &config.posthog {
        if c.enabled {
            fetchers.push(Box::new(PosthogPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.sentry {
        if c.enabled {
            fetchers.push(Box::new(SentryPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.zendesk {
        if c.enabled {
            fetchers.push(Box::new(ZendeskPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.datadog {
        if c.enabled {
            fetchers.push(Box::new(DatadogPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.github_issues {
        if c.enabled {
            fetchers.push(Box::new(GithubIssuesPoller::from_config(c, github)?));
        }
    }
    if let Some(c) = &config.jira {
        if c.enabled {
            fetchers.push(Box::new(JiraPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.linear {
        if c.enabled {
            fetchers.push(Box::new(LinearPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.slack_channels {
        if c.enabled {
            fetchers.push(Box::new(SlackChannelsPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.asana {
        if c.enabled {
            fetchers.push(Box::new(AsanaPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.trello {
        if c.enabled {
            fetchers.push(Box::new(TrelloPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.intercom {
        if c.enabled {
            fetchers.push(Box::new(IntercomPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.mixpanel {
        if c.enabled {
            fetchers.push(Box::new(MixpanelPoller::from_config(c)?));
        }
    }
    if let Some(c) = &config.openpanel {
        if c.enabled {
            fetchers.push(Box::new(OpenpanelPoller::from_config(c)?));
        }
    }
    Ok(fetchers)
}

// Env-var hygiene in tests (documented choice, see task brief): every test
// that resolves a secret uses a globally unique env var name, so concurrent
// tests never race on set_var/remove_var of the same variable.
#[cfg(test)]
mod tests {
    use super::*;
    use merge0_github::FakeGitHub;

    /// The shipped config file, kept parseable by construction.
    const SHIPPED: &str = include_str!("../../../config/sources.toml");

    #[test]
    fn shipped_sources_toml_parses_with_all_sections_disabled() {
        let config = SourcesConfig::parse(SHIPPED).unwrap();
        for (name, enabled) in [
            ("posthog", config.posthog.as_ref().map(|c| c.enabled)),
            ("sentry", config.sentry.as_ref().map(|c| c.enabled)),
            ("zendesk", config.zendesk.as_ref().map(|c| c.enabled)),
            ("datadog", config.datadog.as_ref().map(|c| c.enabled)),
            (
                "github_issues",
                config.github_issues.as_ref().map(|c| c.enabled),
            ),
            ("jira", config.jira.as_ref().map(|c| c.enabled)),
            ("linear", config.linear.as_ref().map(|c| c.enabled)),
            (
                "slack_channels",
                config.slack_channels.as_ref().map(|c| c.enabled),
            ),
            ("asana", config.asana.as_ref().map(|c| c.enabled)),
            ("trello", config.trello.as_ref().map(|c| c.enabled)),
            ("intercom", config.intercom.as_ref().map(|c| c.enabled)),
            ("mixpanel", config.mixpanel.as_ref().map(|c| c.enabled)),
            ("openpanel", config.openpanel.as_ref().map(|c| c.enabled)),
        ] {
            assert_eq!(enabled, Some(false), "section [{name}] missing or enabled");
        }
    }

    #[test]
    fn disabled_sections_are_excluded_from_build_fetchers() {
        // All shipped sections disabled → no fetchers, and no env vars are
        // even resolved (no *_env vars exist in this test environment).
        let config = SourcesConfig::parse(SHIPPED).unwrap();
        let fetchers = build_fetchers(&config, Arc::new(FakeGitHub::new())).unwrap();
        assert!(fetchers.is_empty());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = SourcesConfig::parse("[sentry]\ntoken = \"inline-secret\"\n").unwrap_err();
        assert!(matches!(err, FetchError::Config(_)), "{err:?}");
        let err = SourcesConfig::parse("[not_a_vendor]\nx = 1\n").unwrap_err();
        assert!(matches!(err, FetchError::Config(_)), "{err:?}");
    }

    #[test]
    fn enabled_defaults_to_true_and_base_urls_default() {
        let config = SourcesConfig::parse(
            r#"
            [sentry]
            organization = "acme"
            project = "app"
            auth_token_env = "MERGE0_TEST_CFG_SENTRY_TOKEN_A"
            "#,
        )
        .unwrap();
        let sentry = config.sentry.unwrap();
        assert!(sentry.enabled);
        assert_eq!(sentry.base_url, "https://sentry.io");
    }

    #[test]
    fn zendesk_base_url_derives_from_subdomain_unless_overridden() {
        let base = |toml: &str| {
            SourcesConfig::parse(toml)
                .unwrap()
                .zendesk
                .unwrap()
                .effective_base_url()
        };
        let minimal = r#"
            [zendesk]
            subdomain = "acme"
            email_env = "E"
            api_token_env = "T"
            agent_base_url = "https://acme.zendesk.com/agent"
        "#;
        assert_eq!(base(minimal), "https://acme.zendesk.com");
        let overridden = format!("{minimal}base_url = \"http://127.0.0.1:9\"\n");
        assert_eq!(base(&overridden), "http://127.0.0.1:9");
    }

    #[test]
    fn enabled_source_with_missing_env_var_fails_naming_the_variable() {
        let config = SourcesConfig::parse(
            r#"
            [datadog]
            api_key_env = "MERGE0_TEST_CFG_DD_API_KEY_UNSET"
            app_key_env = "MERGE0_TEST_CFG_DD_APP_KEY_UNSET"
            app_base_url = "https://app.datadoghq.com"
            "#,
        )
        .unwrap();
        let err = build_fetchers(&config, Arc::new(FakeGitHub::new()))
            .err()
            .expect("build must fail");
        match err {
            FetchError::Config(message) => assert!(
                message.contains("MERGE0_TEST_CFG_DD_API_KEY_UNSET"),
                "{message}"
            ),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn enabled_sources_with_resolvable_env_vars_build() {
        // Unique var names per the hygiene note above.
        std::env::set_var("MERGE0_TEST_CFG_BUILD_PH_KEY", "phk");
        std::env::set_var("MERGE0_TEST_CFG_BUILD_SENTRY_TOKEN", "st");
        let config = SourcesConfig::parse(
            r#"
            [posthog]
            project_id = "1"
            api_key_env = "MERGE0_TEST_CFG_BUILD_PH_KEY"
            project_base_url = "https://us.posthog.com/project/1"

            [sentry]
            organization = "acme"
            project = "app"
            auth_token_env = "MERGE0_TEST_CFG_BUILD_SENTRY_TOKEN"

            [github_issues]
            repo = "acme/app"
            "#,
        )
        .unwrap();
        let fetchers = build_fetchers(&config, Arc::new(FakeGitHub::new())).unwrap();
        let names: Vec<_> = fetchers.iter().map(|f| f.source_name()).collect();
        assert_eq!(names, vec!["posthog", "sentry", "github_issues"]);
    }

    #[test]
    fn bad_repo_ref_is_a_config_error() {
        let config = SourcesConfig::parse("[github_issues]\nrepo = \"not-a-repo\"\n").unwrap();
        let err = build_fetchers(&config, Arc::new(FakeGitHub::new()))
            .err()
            .expect("build must fail");
        assert!(matches!(err, FetchError::Config(_)), "{err:?}");
    }
}
