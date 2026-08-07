//! Datadog poller (PRD §1): Events API v2, feeding `merge0-adapter-datadog`.
//!
//! Cursoring: time-window based — `filter[from]={cursor}` (first run:
//! `now - 24h`), and the next cursor is `now` as RFC3339. The
//! fingerprint-deduped upsert absorbs the window overlap between rounds.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::DatadogConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use merge0_adapter_datadog::DatadogAdapter;

pub struct DatadogPoller {
    api_key: Secret,
    app_key: Secret,
    base_url: String,
    app_base_url: String,
    client: reqwest::Client,
}

impl DatadogPoller {
    pub fn from_config(config: &DatadogConfig) -> Result<Self, FetchError> {
        Ok(DatadogPoller {
            api_key: Secret::from_env(&config.api_key_env)?,
            app_key: Secret::from_env(&config.app_key_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            app_base_url: config.app_base_url.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for DatadogPoller {
    fn source_name(&self) -> &'static str {
        "datadog"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(DatadogAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let from = cursor.map(String::from).unwrap_or_else(|| {
            (now - Duration::hours(24)).to_rfc3339_opts(SecondsFormat::Secs, true)
        });
        let url = format!("{}/api/v2/events", self.base_url);
        let payload = json_body(
            checked_send(
                self.client
                    .get(&url)
                    .query(&[("filter[from]", from.as_str())])
                    .header("DD-API-KEY", self.api_key.expose_for_auth_header())
                    .header("DD-APPLICATION-KEY", self.app_key.expose_for_auth_header()),
            )
            .await?,
        )
        .await?;

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "events",
                serde_json::json!({ "app_base_url": self.app_base_url }),
                payload,
            )],
            next_cursor: Some(now.to_rfc3339_opts(SecondsFormat::Secs, true)),
        })
    }
}
