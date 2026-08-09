//! Reddit poller: new posts from operator-designated subreddits, feeding
//! `merge0-adapter-reddit`.
//!
//! Auth is the OAuth2 client-credentials exchange for a "script" app
//! (HTTP Basic with the app id/secret against `/api/v1/access_token`,
//! then a Bearer against the oauth host). Reddit throttles generic
//! User-Agents, so the UA is configured and always sent.
//!
//! Cursoring: the newest post fullname seen (`t3_…`) is stored and passed
//! back as the listing's `before` parameter, so each round reads only
//! what's new. Overlap or a deleted anchor is harmless — fingerprints
//! dedupe downstream — and when `before` yields nothing the cursor is
//! kept, not cleared.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::RedditConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_reddit::RedditAdapter;

pub struct RedditPoller {
    subreddits: Vec<String>,
    client_id: String,
    client_secret: Secret,
    user_agent: String,
    base_url: String,
    auth_base_url: String,
    public_base_url: String,
    client: reqwest::Client,
}

impl RedditPoller {
    pub fn from_config(config: &RedditConfig) -> Result<Self, FetchError> {
        if config.subreddits.is_empty() {
            // No subreddits = nothing this source could ever emit. Fail
            // loudly at startup instead of shipping a dead source.
            return Err(FetchError::Config(
                "reddit is enabled but subreddits is empty — name the communities \
                 to watch (e.g. [\"acmeapp\"]) or disable the source"
                    .into(),
            ));
        }
        Ok(RedditPoller {
            subreddits: config.subreddits.clone(),
            client_id: std::env::var(&config.client_id_env).map_err(|_| {
                FetchError::Config(format!(
                    "environment variable {} is not set",
                    config.client_id_env
                ))
            })?,
            client_secret: Secret::from_env(&config.client_secret_env)?,
            user_agent: config.user_agent.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            auth_base_url: config.auth_base_url.trim_end_matches('/').to_string(),
            public_base_url: config.public_base_url.trim_end_matches('/').to_string(),
            client: http_client()?,
        })
    }

    /// Client-credentials exchange; the token lives only for this round.
    async fn access_token(&self) -> Result<String, FetchError> {
        let response = json_body(
            checked_send(
                self.client
                    .post(format!("{}/api/v1/access_token", self.auth_base_url))
                    .basic_auth(
                        &self.client_id,
                        Some(self.client_secret.expose_for_auth_header()),
                    )
                    .header("user-agent", &self.user_agent)
                    .form(&[("grant_type", "client_credentials")]),
            )
            .await?,
        )
        .await?;
        response["access_token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| {
                FetchError::Transport("reddit token response carried no access_token".into())
            })
    }
}

#[async_trait]
impl Fetcher for RedditPoller {
    fn source_name(&self) -> &'static str {
        "reddit"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(RedditAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let token = self.access_token().await?;
        // One multireddit request covers every watched community.
        let multi = self.subreddits.join("+");
        let mut query: Vec<(&str, String)> = vec![("limit", "100".into())];
        if let Some(before) = cursor {
            query.push(("before", before.to_string()));
        }
        let listing = json_body(
            checked_send(
                self.client
                    .get(format!("{}/r/{}/new", self.base_url, multi))
                    .query(&query)
                    .bearer_auth(&token)
                    .header("user-agent", &self.user_agent),
            )
            .await?,
        )
        .await?;

        // Newest child's fullname becomes next round's `before`. An empty
        // page keeps the old cursor — never step backwards.
        let next_cursor = listing["data"]["children"]
            .as_array()
            .and_then(|children| children.first())
            .and_then(|child| child["data"]["name"].as_str())
            .map(String::from)
            .or_else(|| cursor.map(String::from));

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "subreddit_new",
                serde_json::json!({ "base_url": self.public_base_url }),
                listing,
            )],
            next_cursor,
        })
    }
}
