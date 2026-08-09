//! X (Twitter) poller: v2 recent search over the operator's query —
//! mentions of their handle, watched hashtags, or both — feeding
//! `merge0-adapter-x`.
//!
//! Auth is the app-only Bearer token. Cursoring: `meta.newest_id` is
//! stored and passed back as `since_id`, so each round reads only newer
//! posts; within a round, `next_token` pages are followed up to a cap and
//! the remainder arrives next round via `since_id`.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::XConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_x::XAdapter;

/// Pages fetched per round (at `max_results=100` posts each). A viral
/// spike larger than this drains across rounds via `since_id` rather than
/// in one unbounded burst.
const MAX_PAGES_PER_ROUND: u32 = 5;

pub struct XPoller {
    query: String,
    bearer_token: Secret,
    base_url: String,
    client: reqwest::Client,
}

impl XPoller {
    pub fn from_config(config: &XConfig) -> Result<Self, FetchError> {
        if config.query.trim().is_empty() {
            // No query = nothing this source could ever emit. Fail loudly
            // at startup instead of shipping a dead source.
            return Err(FetchError::Config(
                "x is enabled but query is empty — set the mentions/hashtags to \
                 watch (e.g. \"@acmeapp OR #acmeapp\") or disable the source"
                    .into(),
            ));
        }
        Ok(XPoller {
            query: config.query.clone(),
            bearer_token: Secret::from_env(&config.bearer_token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client: http_client()?,
        })
    }

    async fn fetch_page(
        &self,
        since_id: Option<&str>,
        next_token: Option<&str>,
    ) -> Result<serde_json::Value, FetchError> {
        let mut query: Vec<(&str, String)> = vec![
            ("query", self.query.clone()),
            ("max_results", "100".into()),
            (
                "tweet.fields",
                "id,text,author_id,created_at,public_metrics".into(),
            ),
            ("expansions", "author_id".into()),
            ("user.fields", "id,name,username".into()),
        ];
        if let Some(since) = since_id {
            query.push(("since_id", since.to_string()));
        }
        if let Some(token) = next_token {
            query.push(("next_token", token.to_string()));
        }
        json_body(
            checked_send(
                self.client
                    .get(format!("{}/2/tweets/search/recent", self.base_url))
                    .query(&query)
                    .bearer_auth(self.bearer_token.expose_for_auth_header()),
            )
            .await?,
        )
        .await
    }
}

#[async_trait]
impl Fetcher for XPoller {
    fn source_name(&self) -> &'static str {
        "x"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(XAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let mut data: Vec<serde_json::Value> = Vec::new();
        let mut users: Vec<serde_json::Value> = Vec::new();
        let mut newest_id: Option<String> = None;
        let mut next_token: Option<String> = None;
        let mut page = 0u32;
        loop {
            let response = self.fetch_page(cursor, next_token.as_deref()).await?;
            if let Some(batch) = response["data"].as_array() {
                data.extend(batch.iter().cloned());
            }
            if let Some(included) = response["includes"]["users"].as_array() {
                users.extend(included.iter().cloned());
            }
            // The first page carries the round's newest id.
            if newest_id.is_none() {
                newest_id = response["meta"]["newest_id"].as_str().map(String::from);
            }
            next_token = response["meta"]["next_token"].as_str().map(String::from);
            page += 1;
            if next_token.is_none() {
                break;
            }
            if page >= MAX_PAGES_PER_ROUND {
                tracing::warn!(
                    fetched_pages = page,
                    "x recent search capped this round; since_id resumes the rest"
                );
                break;
            }
        }

        let result_count = data.len();
        Ok(FetchBatch {
            envelopes: vec![envelope(
                "recent_search",
                serde_json::json!({ "query": self.query }),
                serde_json::json!({
                    "data": data,
                    "includes": { "users": users },
                    "meta": { "result_count": result_count },
                }),
            )],
            // An empty round (no newer posts) keeps the old cursor.
            next_cursor: newest_id.or_else(|| cursor.map(String::from)),
        })
    }
}
