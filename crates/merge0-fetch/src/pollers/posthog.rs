//! PostHog poller (PRD §1, Phase 0): error tracking issues + `$rageclick`
//! events, feeding `merge0-adapter-posthog`.
//!
//! Cursoring: the error-tracking issue list is a rolling snapshot (the
//! fingerprint-deduped upsert absorbs re-reads); the events query is
//! incremental via `after={latest event timestamp seen}` (ISO). A round that
//! sees no events keeps the previous cursor.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::PosthogConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_posthog::PosthogAdapter;

pub struct PosthogPoller {
    project_id: String,
    api_key: Secret,
    base_url: String,
    project_base_url: String,
    client: reqwest::Client,
}

impl PosthogPoller {
    pub fn from_config(config: &PosthogConfig) -> Result<Self, FetchError> {
        Ok(PosthogPoller {
            project_id: config.project_id.clone(),
            api_key: Secret::from_env(&config.api_key_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            project_base_url: config.project_base_url.clone(),
            client: http_client()?,
        })
    }

    fn context(&self) -> serde_json::Value {
        serde_json::json!({ "project_base_url": self.project_base_url })
    }
}

#[async_trait]
impl Fetcher for PosthogPoller {
    fn source_name(&self) -> &'static str {
        "posthog"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(PosthogAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let issues_url = format!(
            "{}/api/projects/{}/error_tracking/issues",
            self.base_url, self.project_id
        );
        let issues = json_body(
            checked_send(
                self.client
                    .get(&issues_url)
                    .bearer_auth(self.api_key.expose_for_auth_header()),
            )
            .await?,
        )
        .await?;

        let events_url = format!("{}/api/projects/{}/events", self.base_url, self.project_id);
        let mut query: Vec<(&str, String)> = vec![("event", "$rageclick".into())];
        if let Some(after) = cursor {
            query.push(("after", after.to_string()));
        }
        let events = json_body(
            checked_send(
                self.client
                    .get(&events_url)
                    .query(&query)
                    .bearer_auth(self.api_key.expose_for_auth_header()),
            )
            .await?,
        )
        .await?;

        let next_cursor = latest_event_timestamp(&events).or_else(|| cursor.map(String::from));

        Ok(FetchBatch {
            envelopes: vec![
                envelope("error_tracking_issues", self.context(), issues),
                envelope("rageclick_events", self.context(), events),
            ],
            next_cursor,
        })
    }
}

/// The latest `timestamp` among `results[]`, returned as the vendor's own
/// ISO string (compared as parsed instants, emitted verbatim — no
/// re-formatting precision loss).
fn latest_event_timestamp(events_response: &serde_json::Value) -> Option<String> {
    events_response["results"]
        .as_array()?
        .iter()
        .filter_map(|event| {
            let raw = event["timestamp"].as_str()?;
            let parsed: DateTime<Utc> = raw.parse().ok()?;
            Some((parsed, raw.to_string()))
        })
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, raw)| raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_event_timestamp_picks_max_and_handles_absence() {
        let response = serde_json::json!({ "results": [
            { "timestamp": "2026-08-05T14:00:00Z" },
            { "timestamp": "2026-08-05T16:00:00Z" },
            { "timestamp": "2026-08-05T15:30:00Z" },
            { "no_timestamp": true },
        ]});
        assert_eq!(
            latest_event_timestamp(&response).as_deref(),
            Some("2026-08-05T16:00:00Z")
        );
        assert_eq!(
            latest_event_timestamp(&serde_json::json!({ "results": [] })),
            None
        );
        assert_eq!(latest_event_timestamp(&serde_json::json!({})), None);
    }
}
