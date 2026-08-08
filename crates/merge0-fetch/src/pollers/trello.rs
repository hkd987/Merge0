//! Trello poller: open cards per configured board, feeding
//! `merge0-adapter-trello`.
//!
//! Cursoring: Trello's list-cards API has no `since` filter worth relying
//! on, so every poll fetches the boards' OPEN cards (bounded by board
//! size) and ingest dedupes by fingerprint; the cursor records the poll
//! instant for observability only. Auth: key + token as query params
//! (Trello's scheme).

use super::{checked_send, envelope, http_client, json_body};
use crate::config::TrelloConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_trello::TrelloAdapter;

pub struct TrelloPoller {
    key: Secret,
    token: Secret,
    base_url: String,
    board_ids: Vec<String>,
    client: reqwest::Client,
}

impl TrelloPoller {
    pub fn from_config(config: &TrelloConfig) -> Result<Self, FetchError> {
        if config.board_ids.is_empty() {
            return Err(FetchError::Config(
                "trello enabled with no board_ids configured".into(),
            ));
        }
        Ok(TrelloPoller {
            key: Secret::from_env(&config.key_env)?,
            token: Secret::from_env(&config.token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            board_ids: config.board_ids.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for TrelloPoller {
    fn source_name(&self) -> &'static str {
        "trello"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(TrelloAdapter)
    }

    async fn fetch(
        &self,
        _cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let mut envelopes = Vec::new();
        for board in &self.board_ids {
            let payload = json_body(
                checked_send(self.client.get(format!(
                    "{}/boards/{board}/cards/open?fields=name,desc,dateLastActivity,closed,shortUrl,labels,start&key={}&token={}",
                    self.base_url,
                    self.key.expose_for_auth_header(),
                    self.token.expose_for_auth_header(),
                )))
                .await?,
            )
            .await?;
            envelopes.push(envelope("cards", serde_json::json!({}), payload));
        }
        Ok(FetchBatch {
            envelopes,
            next_cursor: Some(now.to_rfc3339()),
        })
    }
}
