//! Jira poller: JQL search over the Cloud REST API, feeding
//! `merge0-adapter-jira`.
//!
//! Cursoring: the cursor is the last poll's ISO timestamp; it is ANDed
//! onto the configured JQL as `updated >= "yyyy-MM-dd HH:mm"` (Jira's JQL
//! datetime format, minute precision — overlap beats gaps, and ingest
//! upserts are idempotent by fingerprint). Auth is basic (email + API
//! token), Jira Cloud's API-token scheme.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::JiraConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_jira::JiraAdapter;

pub struct JiraPoller {
    email: String,
    api_token: Secret,
    base_url: String,
    jql: String,
    client: reqwest::Client,
}

impl JiraPoller {
    pub fn from_config(config: &JiraConfig) -> Result<Self, FetchError> {
        Ok(JiraPoller {
            email: Secret::from_env(&config.email_env)?
                .expose_for_auth_header()
                .to_string(),
            api_token: Secret::from_env(&config.api_token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            jql: config.jql.clone(),
            client: http_client()?,
        })
    }

    /// The configured JQL with the cursor bound ANDed in front of any
    /// ORDER BY clause.
    fn effective_jql(&self, cursor: Option<&str>) -> String {
        let Some(since) = cursor else {
            return self.jql.clone();
        };
        let (conditions, order) = match self.jql.to_uppercase().find("ORDER BY") {
            Some(index) => (self.jql[..index].trim(), &self.jql[index..]),
            None => (self.jql.trim(), ""),
        };
        let conditions = if conditions.is_empty() {
            format!("updated >= \"{since}\"")
        } else {
            format!("({conditions}) AND updated >= \"{since}\"")
        };
        format!("{conditions} {order}").trim().to_string()
    }
}

#[async_trait]
impl Fetcher for JiraPoller {
    fn source_name(&self) -> &'static str {
        "jira"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(JiraAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let url = format!("{}/rest/api/3/search/jql", self.base_url);
        let payload = json_body(
            checked_send(
                self.client
                    .get(&url)
                    .query(&[
                        ("jql", self.effective_jql(cursor).as_str()),
                        ("maxResults", "100"),
                        (
                            "fields",
                            "summary,description,priority,status,created,updated",
                        ),
                    ])
                    .basic_auth(&self.email, Some(self.api_token.expose_for_auth_header())),
            )
            .await?,
        )
        .await?;

        Ok(FetchBatch {
            envelopes: vec![envelope(
                "issues",
                serde_json::json!({
                    "browse_base_url": format!("{}/browse", self.base_url),
                }),
                payload,
            )],
            // JQL datetime literals are minute-precision "yyyy-MM-dd HH:mm".
            next_cursor: Some(now.format("%Y-%m-%d %H:%M").to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poller(jql: &str) -> JiraPoller {
        JiraPoller {
            email: "bot@example.com".into(),
            api_token: Secret::new("t"),
            base_url: "https://acme-example.atlassian.net".into(),
            jql: jql.into(),
            client: http_client().unwrap(),
        }
    }

    #[test]
    fn cursor_bound_is_anded_before_order_by() {
        let p = poller("statusCategory != Done ORDER BY updated ASC");
        assert_eq!(
            p.effective_jql(Some("2026-08-01 12:00")),
            "(statusCategory != Done) AND updated >= \"2026-08-01 12:00\" ORDER BY updated ASC"
        );
    }

    #[test]
    fn cursor_bound_without_order_by_and_without_cursor() {
        let p = poller("project = CHK");
        assert_eq!(
            p.effective_jql(Some("2026-08-01 12:00")),
            "(project = CHK) AND updated >= \"2026-08-01 12:00\""
        );
        assert_eq!(p.effective_jql(None), "project = CHK");
    }
}
