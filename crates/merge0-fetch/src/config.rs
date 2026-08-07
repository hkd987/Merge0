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
    DatadogPoller, GithubIssuesPoller, PosthogPoller, SentryPoller, ZendeskPoller,
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
