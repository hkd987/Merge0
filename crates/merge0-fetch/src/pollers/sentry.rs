//! Sentry poller (PRD §1, Phase 0): unresolved issues, feeding
//! `merge0-adapter-sentry`.
//!
//! Cursoring: Sentry's own pagination cursor, parsed from the `Link`
//! response header — the `rel="next"` entry with `results="true"` carries
//! `cursor="..."`. No next page (absent header, absent entry, or
//! `results="false"`) clears the cursor so the next round restarts from the
//! newest issues.

use super::{checked_send, envelope, http_client, json_body};
use crate::config::SentryConfig;
use crate::{FetchBatch, FetchError, Fetcher, Secret};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_adapter_sentry::SentryAdapter;

pub struct SentryPoller {
    organization: String,
    project: String,
    auth_token: Secret,
    base_url: String,
    client: reqwest::Client,
}

impl SentryPoller {
    pub fn from_config(config: &SentryConfig) -> Result<Self, FetchError> {
        Ok(SentryPoller {
            organization: config.organization.clone(),
            project: config.project.clone(),
            auth_token: Secret::from_env(&config.auth_token_env)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl Fetcher for SentryPoller {
    fn source_name(&self) -> &'static str {
        "sentry"
    }

    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter> {
        Box::new(SentryAdapter)
    }

    async fn fetch(
        &self,
        cursor: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError> {
        let url = format!(
            "{}/api/0/projects/{}/{}/issues/",
            self.base_url, self.organization, self.project
        );
        let mut query: Vec<(&str, String)> = vec![("query", "is:unresolved".into())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        let response = checked_send(
            self.client
                .get(&url)
                .query(&query)
                .bearer_auth(self.auth_token.expose_for_auth_header()),
        )
        .await?;

        // Read the Link header before consuming the body.
        let next_cursor = response
            .headers()
            .get("link")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_next_cursor);
        let payload = json_body(response).await?;

        Ok(FetchBatch {
            // The Sentry adapter needs no context: issue payloads carry
            // their own `permalink`.
            envelopes: vec![envelope("issues", serde_json::json!({}), payload)],
            next_cursor,
        })
    }
}

/// Lenient parse of Sentry's `Link` header: find the `rel="next"` entry,
/// honor its `results` flag, extract `cursor="..."`. Anything unexpected →
/// `None` (treated as "no next page", never an error).
fn parse_next_cursor(link_header: &str) -> Option<String> {
    for entry in link_header.split(',') {
        if !entry.contains(r#"rel="next""#) {
            continue;
        }
        if !entry.contains(r#"results="true""#) {
            return None;
        }
        let start = entry.find(r#"cursor=""#)? + r#"cursor=""#.len();
        let rest = &entry[start..];
        let end = rest.find('"')?;
        let cursor = &rest[..end];
        return (!cursor.is_empty()).then(|| cursor.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = concat!(
        r#"<https://sentry.example.com/api/0/projects/o/p/issues/?&cursor=100:0:1>; "#,
        r#"rel="previous"; results="false"; cursor="100:0:1", "#,
        r#"<https://sentry.example.com/api/0/projects/o/p/issues/?&cursor=100:100:0>; "#,
        r#"rel="next"; results="true"; cursor="100:100:0""#
    );

    #[test]
    fn parses_next_cursor_from_full_header() {
        assert_eq!(parse_next_cursor(FULL).as_deref(), Some("100:100:0"));
    }

    #[test]
    fn results_false_means_no_next_page() {
        let done = FULL.replace(
            r#"rel="next"; results="true""#,
            r#"rel="next"; results="false""#,
        );
        assert_eq!(parse_next_cursor(&done), None);
    }

    #[test]
    fn absent_or_garbled_header_is_none_not_an_error() {
        assert_eq!(parse_next_cursor(""), None);
        assert_eq!(parse_next_cursor("not a link header"), None);
        assert_eq!(
            parse_next_cursor(r#"<u>; rel="previous"; results="true"; cursor="x""#),
            None
        );
        // next entry present but cursor attribute missing/empty
        assert_eq!(
            parse_next_cursor(r#"<u>; rel="next"; results="true""#),
            None
        );
        assert_eq!(
            parse_next_cursor(r#"<u>; rel="next"; results="true"; cursor="""#),
            None
        );
    }
}
