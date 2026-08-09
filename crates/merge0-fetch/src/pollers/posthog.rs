//! PostHog poller (PRD §1, Phase 0): error tracking issues, `$rageclick` +
//! `$dead_click` events, and (when configured) funnel insights, feeding
//! `merge0-adapter-posthog`.
//!
//! Cursoring: the error-tracking issue list and funnel insights are rolling
//! snapshots (the fingerprint-deduped upsert absorbs re-reads); the events
//! queries are incremental via `after={latest event timestamp seen across
//! both event families}` (ISO). A round that sees no events keeps the
//! previous cursor.
//!
//! Funnels: one GET per configured insight id; the responses are assembled
//! into the `funnels` envelope's `{"results": [...]}` payload (each entry a
//! vendor insight object, verbatim). No configured ids → no envelope.

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
    funnel_insight_ids: Vec<String>,
    client: reqwest::Client,
}

impl PosthogPoller {
    pub fn from_config(config: &PosthogConfig) -> Result<Self, FetchError> {
        Ok(PosthogPoller {
            project_id: config.project_id.clone(),
            api_key: Secret::from_env(&config.api_key_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            project_base_url: config.project_base_url.clone(),
            funnel_insight_ids: config.funnel_insight_ids.clone(),
            client: http_client()?,
        })
    }

    fn context(&self) -> serde_json::Value {
        serde_json::json!({ "project_base_url": self.project_base_url })
    }

    /// One incremental events query (`event=<name>`, optional `after`).
    async fn fetch_events(
        &self,
        event: &str,
        cursor: Option<&str>,
    ) -> Result<serde_json::Value, FetchError> {
        let events_url = format!("{}/api/projects/{}/events", self.base_url, self.project_id);
        let mut query: Vec<(&str, String)> = vec![("event", event.into())];
        if let Some(after) = cursor {
            query.push(("after", after.to_string()));
        }
        json_body(
            checked_send(
                self.client
                    .get(&events_url)
                    .query(&query)
                    .bearer_auth(self.api_key.expose_for_auth_header()),
            )
            .await?,
        )
        .await
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

        let rageclicks = self.fetch_events("$rageclick", cursor).await?;
        let dead_clicks = self.fetch_events("$dead_click", cursor).await?;

        let next_cursor = latest_event_timestamp([&rageclicks, &dead_clicks])
            .or_else(|| cursor.map(String::from));

        let mut envelopes = vec![
            envelope("error_tracking_issues", self.context(), issues),
            envelope("rageclick_events", self.context(), rageclicks),
            envelope("dead_click_events", self.context(), dead_clicks),
        ];

        if !self.funnel_insight_ids.is_empty() {
            let mut insights = Vec::with_capacity(self.funnel_insight_ids.len());
            for insight_id in &self.funnel_insight_ids {
                let insight_url = format!(
                    "{}/api/projects/{}/insights/{insight_id}",
                    self.base_url, self.project_id
                );
                insights.push(
                    json_body(
                        checked_send(
                            self.client
                                .get(&insight_url)
                                .bearer_auth(self.api_key.expose_for_auth_header()),
                        )
                        .await?,
                    )
                    .await?,
                );
            }
            envelopes.push(envelope(
                "funnels",
                self.context(),
                serde_json::json!({ "results": insights }),
            ));
        }

        Ok(FetchBatch {
            envelopes,
            next_cursor,
        })
    }
}

/// The latest `timestamp` among the given responses' `results[]`, returned
/// as the vendor's own ISO string (compared as parsed instants, emitted
/// verbatim — no re-formatting precision loss).
fn latest_event_timestamp<'a>(
    responses: impl IntoIterator<Item = &'a serde_json::Value>,
) -> Option<String> {
    responses
        .into_iter()
        .filter_map(|response| response["results"].as_array())
        .flatten()
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
    fn latest_event_timestamp_picks_max_across_responses_and_handles_absence() {
        let rageclicks = serde_json::json!({ "results": [
            { "timestamp": "2026-08-05T14:00:00Z" },
            { "timestamp": "2026-08-05T16:00:00Z" },
            { "no_timestamp": true },
        ]});
        let dead_clicks = serde_json::json!({ "results": [
            { "timestamp": "2026-08-05T17:00:00Z" },
        ]});
        assert_eq!(
            latest_event_timestamp([&rageclicks]).as_deref(),
            Some("2026-08-05T16:00:00Z")
        );
        assert_eq!(
            latest_event_timestamp([&rageclicks, &dead_clicks]).as_deref(),
            Some("2026-08-05T17:00:00Z"),
            "the cursor spans both event families"
        );
        assert_eq!(
            latest_event_timestamp([&serde_json::json!({ "results": [] })]),
            None
        );
        assert_eq!(latest_event_timestamp([&serde_json::json!({})]), None);
    }
}
