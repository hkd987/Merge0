//! Slack channels poller: `conversations.history` for each designated
//! channel (e.g. #bugs), feeding `merge0-adapter-slack`.
//!
//! This is the INGEST side of Slack — separate from the notification
//! webhook. Cursoring: the newest message `ts` seen per poll, applied as
//! `oldest` (exclusive) on the next; one cursor covers all channels
//! (Slack `ts` values are epoch-based and comparable across channels —
//! overlap beats gaps, ingest is idempotent by fingerprint).

use super::{checked_send, envelope, http_client, json_body};
use crate::config::{SlackChannel, SlackChannelsConfig};
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_slack::SlackAdapter;

pub struct SlackChannelsPoller {
    bot_token: Secret,
    base_url: String,
    team_base_url: String,
    channels: Vec<SlackChannel>,
    client: reqwest::Client,
}

impl SlackChannelsPoller {
    pub fn from_config(config: &SlackChannelsConfig) -> Result<Self, FetchError> {
        if config.channels.is_empty() {
            return Err(FetchError::Config(
                "slack_channels enabled with no channels configured".into(),
            ));
        }
        Ok(SlackChannelsPoller {
            bot_token: Secret::from_env(&config.bot_token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            team_base_url: config.team_base_url.trim_end_matches('/').to_string(),
            channels: config.channels.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for SlackChannelsPoller {
    fn source_name(&self) -> &'static str {
        "slack_channels"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(SlackAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let mut envelopes = Vec::new();
        let mut newest_ts: Option<String> = cursor.map(String::from);

        for channel in &self.channels {
            let mut query = vec![("channel", channel.id.clone()), ("limit", "200".into())];
            if let Some(oldest) = cursor {
                query.push(("oldest", oldest.to_string()));
            }
            let payload = json_body(
                checked_send(
                    self.client
                        .get(format!("{}/conversations.history", self.base_url))
                        .query(&query)
                        .bearer_auth(self.bot_token.expose_for_auth_header()),
                )
                .await?,
            )
            .await?;

            // Slack errors arrive with HTTP 200 + {"ok": false}.
            if payload["ok"].as_bool() != Some(true) {
                return Err(FetchError::Api {
                    status: 200,
                    message: format!(
                        "conversations.history #{}: {}",
                        channel.name,
                        payload["error"].as_str().unwrap_or("unknown error")
                    ),
                });
            }

            // Track the newest ts across channels for the next cursor
            // (string compare works: same-epoch-width fixed format).
            if let Some(messages) = payload["messages"].as_array() {
                for message in messages {
                    if let Some(ts) = message["ts"].as_str() {
                        if newest_ts.as_deref().map(|cur| ts > cur).unwrap_or(true) {
                            newest_ts = Some(ts.to_string());
                        }
                    }
                }
            }

            envelopes.push(envelope(
                "messages",
                serde_json::json!({
                    "team_base_url": self.team_base_url,
                    "channel_id": channel.id,
                    "channel_name": channel.name,
                }),
                payload,
            ));
        }

        Ok(FetchBatch {
            envelopes,
            next_cursor: newest_ts,
        })
    }
}
