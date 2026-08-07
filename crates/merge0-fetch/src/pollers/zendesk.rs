//! Zendesk poller (PRD §1, P1): incremental ticket export, feeding
//! `merge0-adapter-zendesk`.
//!
//! Cursoring: Zendesk's incremental API is cursor-native — request
//! `start_time={cursor or 0}` and persist the response's `end_time` for the
//! next round. Auth is the API-token flavor of Basic auth:
//! username `{email}/token`, password the token.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::ZendeskConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_zendesk::ZendeskAdapter;

pub struct ZendeskPoller {
    /// The agent email is an identifier, not a credential, but it is still
    /// resolved from the environment like the token (config carries names
    /// only, uniformly).
    email: String,
    api_token: Secret,
    base_url: String,
    agent_base_url: String,
    client: reqwest::Client,
}

impl ZendeskPoller {
    pub fn from_config(config: &ZendeskConfig) -> Result<Self, FetchError> {
        Ok(ZendeskPoller {
            email: Secret::from_env(&config.email_env)?
                .expose_for_auth_header()
                .to_string(),
            api_token: Secret::from_env(&config.api_token_env)?,
            base_url: config
                .effective_base_url()
                .trim_end_matches('/')
                .to_string(),
            agent_base_url: config.agent_base_url.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for ZendeskPoller {
    fn source_name(&self) -> &'static str {
        "zendesk"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(ZendeskAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let url = format!("{}/api/v2/incremental/tickets.json", self.base_url);
        let payload = json_body(
            checked_send(
                self.client
                    .get(&url)
                    .query(&[("start_time", cursor.unwrap_or("0"))])
                    .basic_auth(
                        format!("{}/token", self.email),
                        Some(self.api_token.expose_for_auth_header()),
                    ),
            )
            .await?,
        )
        .await?;

        // `end_time` is a unix epoch integer; keep it as a string cursor.
        let next_cursor = match &payload["end_time"] {
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => cursor.map(String::from),
        };

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "tickets",
                serde_json::json!({ "agent_base_url": self.agent_base_url }),
                payload,
            )],
            next_cursor,
        })
    }
}
