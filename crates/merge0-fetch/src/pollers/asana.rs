//! Asana poller: list tasks per configured project with
//! `modified_since`, feeding `merge0-adapter-asana`.
//!
//! Cursoring: the poll instant (ISO) becomes `modified_since` next round.
//! Auth: personal access token as a bearer.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::AsanaConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_asana::AsanaAdapter;

pub struct AsanaPoller {
    pat: Secret,
    base_url: String,
    project_gids: Vec<String>,
    client: reqwest::Client,
}

const TASK_FIELDS: &str = "gid,name,notes,created_at,modified_at,completed,permalink_url";

impl AsanaPoller {
    pub fn from_config(config: &AsanaConfig) -> Result<Self, FetchError> {
        if config.project_gids.is_empty() {
            return Err(FetchError::Config(
                "asana enabled with no project_gids configured".into(),
            ));
        }
        Ok(AsanaPoller {
            pat: Secret::from_env(&config.pat_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            project_gids: config.project_gids.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for AsanaPoller {
    fn source_name(&self) -> &'static str {
        "asana"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(AsanaAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let mut envelopes = Vec::new();
        for gid in &self.project_gids {
            let mut query = vec![("opt_fields", TASK_FIELDS.to_string())];
            if let Some(since) = cursor {
                query.push(("modified_since", since.to_string()));
            }
            let payload = json_body(
                checked_send(
                    self.client
                        .get(format!("{}/projects/{gid}/tasks", self.base_url))
                        .query(&query)
                        .bearer_auth(self.pat.expose_for_auth_header()),
                )
                .await?,
            )
            .await?;
            envelopes.push(envelope("tasks", serde_json::json!({}), payload));
        }
        Ok(FetchBatch {
            envelopes,
            next_cursor: Some(now.to_rfc3339()),
        })
    }
}
