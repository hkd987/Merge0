//! OpenPanel poller: error-shaped custom events via the export API, feeding
//! `merge0-adapter-openpanel`.
//!
//! OpenPanel is a general product-analytics store with no built-in error
//! tracking, so *which* events are defect signals is an operator decision:
//! `error_events` in `config/sources.toml` names them (e.g. `error`,
//! `payment_failed`), and only those are exported. Authentication is the
//! documented header pair (`openpanel-client-id` / `openpanel-client-secret`)
//! with a **read**-mode client — the default write client cannot use the
//! export API.
//!
//! Cursoring: incremental via `start = latest createdAt seen` (the vendor's
//! own string, passed back verbatim). The first round reads a trailing
//! `lookback_days` window. Pagination follows `meta.pages` up to a per-round
//! cap; a capped round logs what it left behind and the cursor picks the
//! remainder up next round.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::OpenpanelConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use merge0_adapter_openpanel::OpenpanelAdapter;

/// Pages fetched per round (at `limit=1000` events each). A backlog larger
/// than this is drained across rounds by the cursor rather than in one
/// unbounded burst.
const MAX_PAGES_PER_ROUND: u64 = 10;

pub struct OpenpanelPoller {
    project_id: String,
    client_id: String,
    client_secret: Secret,
    base_url: String,
    project_base_url: String,
    error_events: Vec<String>,
    lookback_days: i64,
    client: reqwest::Client,
}

impl OpenpanelPoller {
    pub fn from_config(config: &OpenpanelConfig) -> Result<Self, FetchError> {
        if config.error_events.is_empty() {
            // No named error events = nothing this source could ever emit.
            // Fail loudly at startup instead of shipping a dead source.
            return Err(FetchError::Config(
                "openpanel is enabled but error_events is empty — name the event(s) \
                 that represent defects (e.g. [\"error\", \"payment_failed\"]) or \
                 disable the source"
                    .into(),
            ));
        }
        Ok(OpenpanelPoller {
            project_id: config.project_id.clone(),
            client_id: std::env::var(&config.client_id_env).map_err(|_| {
                FetchError::Config(format!(
                    "environment variable {} is not set",
                    config.client_id_env
                ))
            })?,
            client_secret: Secret::from_env(&config.client_secret_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            project_base_url: config.project_base_url.trim_end_matches('/').to_string(),
            error_events: config.error_events.clone(),
            lookback_days: config.lookback_days,
            client: http_client()?,
        })
    }

    fn context(&self) -> serde_json::Value {
        serde_json::json!({ "project_base_url": self.project_base_url })
    }

    async fn fetch_page(&self, start: &str, page: u64) -> Result<serde_json::Value, FetchError> {
        let mut query: Vec<(&str, String)> = vec![
            ("project_id", self.project_id.clone()),
            ("start", start.to_string()),
            ("page", page.to_string()),
            ("limit", "1000".into()),
        ];
        for event in &self.error_events {
            query.push(("event", event.clone()));
        }
        json_body(
            checked_send(
                self.client
                    .get(format!("{}/export/events", self.base_url))
                    .query(&query)
                    .header("openpanel-client-id", &self.client_id)
                    .header(
                        "openpanel-client-secret",
                        self.client_secret.expose_for_auth_header(),
                    ),
            )
            .await?,
        )
        .await
    }
}

#[async_trait]
impl Fetcher for OpenpanelPoller {
    fn source_name(&self) -> &'static str {
        "openpanel"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(OpenpanelAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let start = cursor
            .map(String::from)
            .unwrap_or_else(|| (now - Duration::days(self.lookback_days)).to_rfc3339());

        let mut data: Vec<serde_json::Value> = Vec::new();
        let mut meta = serde_json::json!({});
        let mut page = 1u64;
        loop {
            let response = self.fetch_page(&start, page).await?;
            let batch = response["data"].as_array().cloned().unwrap_or_default();
            let pages = response["meta"]["pages"].as_u64().unwrap_or(1);
            meta = response["meta"].clone();
            let batch_len = batch.len();
            data.extend(batch);
            if page >= pages || batch_len == 0 {
                break;
            }
            if page >= MAX_PAGES_PER_ROUND {
                tracing::warn!(
                    fetched_pages = page,
                    total_pages = pages,
                    "openpanel export capped this round; the cursor resumes the rest"
                );
                break;
            }
            page += 1;
        }

        // The vendor's own createdAt string, compared as instants and
        // emitted verbatim (no re-formatting precision loss).
        let next_cursor = data
            .iter()
            .filter_map(|event| {
                let raw = event["createdAt"].as_str()?;
                let parsed: DateTime<Utc> = raw.parse().ok()?;
                Some((parsed, raw.to_string()))
            })
            .max_by_key(|(parsed, _)| *parsed)
            .map(|(_, raw)| raw)
            .or_else(|| cursor.map(String::from));

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "events",
                self.context(),
                serde_json::json!({ "meta": meta, "data": data }),
            )],
            next_cursor,
        })
    }
}
