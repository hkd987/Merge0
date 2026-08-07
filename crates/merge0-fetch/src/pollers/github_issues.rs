//! GitHub Issues poller (PRD §1, P1): feeds `merge0-adapter-github-issues`
//! via the shared [`GitHubApi`] trait — never raw HTTP, so it rides the same
//! app auth and stays testable against `FakeGitHub`.
//!
//! Cursoring: `since={cursor}` (RFC3339), next cursor is `now`. The `since`
//! filter is updated-at based and inclusive, so rounds overlap slightly; the
//! fingerprint-deduped upsert absorbs that.

use super::envelope;
use crate::config::GithubIssuesConfig;
use crate::{FetchBatch, FetchError, Fetcher};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use merge0_adapter_github_issues::GithubIssuesAdapter;
use merge0_github::{GitHubApi, GitHubError, RepoRef};
use std::sync::Arc;

pub struct GithubIssuesPoller {
    repo: RepoRef,
    github: Arc<dyn GitHubApi>,
}

impl GithubIssuesPoller {
    pub fn from_config(
        config: &GithubIssuesConfig,
        github: Arc<dyn GitHubApi>,
    ) -> Result<Self, FetchError> {
        Ok(GithubIssuesPoller {
            repo: RepoRef::parse(&config.repo).map_err(|e| FetchError::Config(e.to_string()))?,
            github,
        })
    }
}

#[async_trait]
impl Fetcher for GithubIssuesPoller {
    fn source_name(&self) -> &'static str {
        "github_issues"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(GithubIssuesAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let since = cursor
            .map(|c| {
                DateTime::parse_from_rfc3339(c)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        FetchError::Config(format!("stored github_issues cursor {c:?}: {e}"))
                    })
            })
            .transpose()?;
        let issues = self
            .github
            .list_issues(&self.repo, since)
            .await
            .map_err(|e| match e {
                GitHubError::Api { status, message } => FetchError::Api { status, message },
                other => FetchError::Transport(other.to_string()),
            })?;

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "issues",
                serde_json::json!({ "repo": self.repo.full() }),
                serde_json::Value::Array(issues),
            )],
            next_cursor: Some(now.to_rfc3339_opts(SecondsFormat::Secs, true)),
        })
    }
}
