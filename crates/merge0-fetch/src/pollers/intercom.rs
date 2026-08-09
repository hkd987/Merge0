//! Intercom poller: the conversations search API with an `updated_at`
//! bound, feeding `merge0-adapter-intercom`.
//!
//! Cursoring: unix seconds of the last poll → `updated_at > cursor` in
//! the search body. Auth: access token as a bearer.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::IntercomConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_intercom::IntercomAdapter;

pub struct IntercomPoller {
    access_token: Secret,
    base_url: String,
    app_base_url: String,
    client: reqwest::Client,
}

impl IntercomPoller {
    pub fn from_config(config: &IntercomConfig) -> Result<Self, FetchError> {
        Ok(IntercomPoller {
            access_token: Secret::from_env(&config.access_token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            app_base_url: config.app_base_url.trim_end_matches('/').to_string(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for IntercomPoller {
    fn source_name(&self) -> &'static str {
        "intercom"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(IntercomAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        // First poll: 30-day lookback.
        let since: i64 = cursor
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| (now - chrono::Duration::days(30)).timestamp());
        let body = serde_json::json!({
            "query": { "field": "updated_at", "operator": ">", "value": since },
            "pagination": { "per_page": 100 },
        });
        let payload = json_body(
            checked_send(
                self.client
                    .post(format!("{}/conversations/search", self.base_url))
                    .bearer_auth(self.access_token.expose_for_auth_header())
                    .header("Intercom-Version", "2.11")
                    .json(&body),
            )
            .await?,
        )
        .await?;

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "conversations",
                serde_json::json!({ "app_base_url": self.app_base_url }),
                payload,
            )],
            next_cursor: Some(now.timestamp().to_string()),
        })
    }
}
