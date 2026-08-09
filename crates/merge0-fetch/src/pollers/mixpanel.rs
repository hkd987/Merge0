//! Mixpanel poller: saved-funnel drop-off analysis via the Query API,
//! feeding `merge0-adapter-mixpanel`.
//!
//! Two endpoints per round, both HTTP Basic-authenticated with a service
//! account (username + secret):
//!
//! - `GET /api/query/funnels/list?project_id=` — funnel id → name, so the
//!   Signal can carry a human title (the funnels query response itself
//!   never repeats the name);
//! - `GET /api/query/funnels?project_id=&funnel_id=&from_date=&to_date=` —
//!   one call per configured funnel id, response passed through verbatim.
//!
//! Funnel results are rolling aggregates, not events, so there is no
//! incremental cursor: every round re-reads the trailing `lookback_days`
//! window and the fingerprint-deduped upsert absorbs the re-reads (same
//! posture as the PostHog funnel insights).

use super::{checked_send, envelope, http_client, json_body};
use crate::config::MixpanelConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use merge0_adapter_mixpanel::MixpanelAdapter;
use std::collections::BTreeMap;

pub struct MixpanelPoller {
    project_id: String,
    username: String,
    secret: Secret,
    base_url: String,
    project_base_url: String,
    funnel_ids: Vec<u64>,
    lookback_days: i64,
    client: reqwest::Client,
}

impl MixpanelPoller {
    pub fn from_config(config: &MixpanelConfig) -> Result<Self, FetchError> {
        if config.funnel_ids.is_empty() {
            // A Mixpanel source with no funnels can never produce a signal —
            // fail loudly at startup instead of shipping a silently dead
            // source (the exact incident class the repo hygiene rule about
            // gate-floor clearance exists for).
            return Err(FetchError::Config(
                "mixpanel is enabled but funnel_ids is empty — the funnels query is \
                 its only signal source, so this configuration can never produce one"
                    .into(),
            ));
        }
        Ok(MixpanelPoller {
            project_id: config.project_id.clone(),
            username: std::env::var(&config.service_account_user_env).map_err(|_| {
                FetchError::Config(format!(
                    "environment variable {} is not set",
                    config.service_account_user_env
                ))
            })?,
            secret: Secret::from_env(&config.service_account_secret_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            project_base_url: config.project_base_url.trim_end_matches('/').to_string(),
            funnel_ids: config.funnel_ids.clone(),
            lookback_days: config.lookback_days,
            client: http_client()?,
        })
    }

    fn context(&self) -> serde_json::Value {
        serde_json::json!({ "project_base_url": self.project_base_url })
    }

    async fn get(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<serde_json::Value, FetchError> {
        json_body(
            checked_send(
                self.client
                    .get(url)
                    .query(query)
                    .basic_auth(&self.username, Some(self.secret.expose_for_auth_header())),
            )
            .await?,
        )
        .await
    }
}

#[async_trait]
impl Fetcher for MixpanelPoller {
    fn source_name(&self) -> &'static str {
        "mixpanel"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(MixpanelAdapter)
    }

    async fn fetch(
        &self,
        _cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        // Funnel id → display name. Absence is tolerated per id (a deleted
        // saved funnel must not sink the round), so this is best-effort.
        let names: BTreeMap<u64, String> = self
            .get(
                &format!("{}/api/query/funnels/list", self.base_url),
                &[("project_id", self.project_id.clone())],
            )
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    entry["funnel_id"].as_u64()?,
                    entry["name"].as_str()?.to_string(),
                ))
            })
            .collect();

        let from_date = (now - Duration::days(self.lookback_days)).format("%Y-%m-%d");
        let to_date = now.format("%Y-%m-%d");
        let mut results = Vec::with_capacity(self.funnel_ids.len());
        for funnel_id in &self.funnel_ids {
            let response = self
                .get(
                    &format!("{}/api/query/funnels", self.base_url),
                    &[
                        ("project_id", self.project_id.clone()),
                        ("funnel_id", funnel_id.to_string()),
                        ("from_date", from_date.to_string()),
                        ("to_date", to_date.to_string()),
                    ],
                )
                .await?;
            results.push(serde_json::json!({
                "funnel_id": funnel_id,
                "name": names
                    .get(funnel_id)
                    .cloned()
                    .unwrap_or_else(|| format!("funnel {funnel_id}")),
                "fetched_at": now.to_rfc3339(),
                "response": response,
            }));
        }

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "funnels",
                self.context(),
                serde_json::json!({ "results": results }),
            )],
            next_cursor: None,
        })
    }
}
