//! Linear poller: the GraphQL API's `issues` query, feeding
//! `merge0-adapter-linear`.
//!
//! Cursoring: `updatedAt` of the last poll (ISO instant) — the query
//! filters `updatedAt: { gte: $since }`. Auth is the raw API key in the
//! `Authorization` header (Linear's scheme — no `Bearer` prefix).

use super::{checked_send, envelope, http_client, json_body};
use crate::config::LinearConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_linear::LinearAdapter;

pub struct LinearPoller {
    api_key: Secret,
    base_url: String,
    client: reqwest::Client,
}

const ISSUES_QUERY: &str = "query Issues($since: DateTimeOrDuration) {\
 issues(filter: { updatedAt: { gte: $since } }, first: 100) {\
 nodes { identifier title description priority createdAt updatedAt url state { type } } } }";

impl LinearPoller {
    pub fn from_config(config: &LinearConfig) -> Result<Self, FetchError> {
        Ok(LinearPoller {
            api_key: Secret::from_env(&config.api_key_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for LinearPoller {
    fn source_name(&self) -> &'static str {
        "linear"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(LinearAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        // First poll: look back 30 days rather than paging all history.
        let since = cursor
            .map(String::from)
            .unwrap_or_else(|| (now - chrono::Duration::days(30)).to_rfc3339());
        let body = serde_json::json!({
            "query": ISSUES_QUERY,
            "variables": { "since": since },
        });
        let payload = json_body(
            checked_send(
                self.client
                    .post(format!("{}/graphql", self.base_url))
                    .header("authorization", self.api_key.expose_for_auth_header())
                    .json(&body),
            )
            .await?,
        )
        .await?;

        // GraphQL errors arrive with HTTP 200; fail loudly, never silently
        // ingest nothing.
        if let Some(first_error) = payload["errors"][0]["message"].as_str() {
            return Err(FetchError::Api {
                status: 200,
                message: format!("GraphQL: {first_error}"),
            });
        }

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "issues",
                serde_json::json!({}),
                payload["data"]["issues"].clone(),
            )],
            next_cursor: Some(now.to_rfc3339()),
        })
    }
}
