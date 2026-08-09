//! Jira Cloud story creation (`POST /rest/api/3/issue`).
//!
//! Auth is the same basic email + API-token scheme the Jira poller uses, so
//! a team that already connected Jira for ingestion has nothing new to set
//! up beyond a project key. Credentials arrive as values from the server's
//! env (referenced by *name* in config, per the no-credentials rule) and
//! are never logged — `Debug` is not derived on the client.

use crate::{CreatedStory, Story, Tracker, TrackerError};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct JiraTracker {
    base_url: String,
    email: String,
    api_token: String,
    project_key: String,
    issue_type: String,
    client: reqwest::Client,
}

impl JiraTracker {
    pub fn new(
        base_url: impl Into<String>,
        email: impl Into<String>,
        api_token: impl Into<String>,
        project_key: impl Into<String>,
        issue_type: impl Into<String>,
    ) -> Result<Self, TrackerError> {
        Ok(JiraTracker {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            email: email.into(),
            api_token: api_token.into(),
            project_key: project_key.into(),
            issue_type: issue_type.into(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| TrackerError::Transport(e.to_string()))?,
        })
    }
}

/// Plain text → Atlassian Document Format.
///
/// Jira v3 rejects a plain string description, and ADF text nodes cannot
/// carry newlines, so each line becomes its own paragraph. Blank lines are
/// dropped (ADF has no empty-paragraph concept worth emitting) — the
/// section headings in the body keep it readable without them.
fn to_adf(text: &str) -> Value {
    let paragraphs: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            json!({
                "type": "paragraph",
                "content": [{ "type": "text", "text": line }],
            })
        })
        .collect();
    json!({ "type": "doc", "version": 1, "content": paragraphs })
}

#[async_trait]
impl Tracker for JiraTracker {
    async fn create_story(&self, story: &Story) -> Result<CreatedStory, TrackerError> {
        let url = format!("{}/rest/api/3/issue", self.base_url);
        let body = json!({
            "fields": {
                "project": { "key": self.project_key },
                "issuetype": { "name": self.issue_type },
                "summary": story.title,
                "description": to_adf(&story.description),
                "labels": story.labels,
            }
        });

        let response = self
            .client
            .post(&url)
            .basic_auth(&self.email, Some(&self.api_token))
            .json(&body)
            .send()
            .await
            .map_err(|e| TrackerError::Transport(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            // Jira puts the actionable part (unknown project, bad issue
            // type, missing required field) in the body — carry it through
            // instead of reporting a bare status code.
            let body = response.text().await.unwrap_or_default();
            return Err(TrackerError::Api {
                status: status.as_u16(),
                body: body.chars().take(500).collect(),
            });
        }

        let created: Value = response
            .json()
            .await
            .map_err(|e| TrackerError::Malformed(e.to_string()))?;
        let key = created
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::Malformed("response has no issue key".into()))?;

        Ok(CreatedStory {
            key: key.to_string(),
            // The API returns a REST self-link; a human needs /browse.
            url: format!("{}/browse/{key}", self.base_url),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{basic_auth, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn story() -> Story {
        Story {
            title: "Attendance export drops the last student".into(),
            description: "Line one\n\n## Reproduction\nOpen /classes/roster\nmerge0:report 01ABC"
                .into(),
            labels: vec![merge0_signal::ORIGIN_LABEL.to_string()],
        }
    }

    #[tokio::test]
    async fn creates_the_issue_and_returns_a_browsable_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/issue"))
            .and(basic_auth("bot@example.com", "token-value"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "10042", "key": "ENG-1421",
                "self": "https://acme.atlassian.net/rest/api/3/issue/10042"
            })))
            .mount(&server)
            .await;

        let tracker = JiraTracker::new(
            format!("{}/", server.uri()), // trailing slash must be tolerated
            "bot@example.com",
            "token-value",
            "ENG",
            "Task",
        )
        .unwrap();
        let created = tracker.create_story(&story()).await.unwrap();

        assert_eq!(created.key, "ENG-1421");
        assert_eq!(created.url, format!("{}/browse/ENG-1421", server.uri()));

        // The request carried project, type, labels, and an ADF body.
        let request = &server.received_requests().await.unwrap()[0];
        let sent: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(sent["fields"]["project"]["key"], "ENG");
        assert_eq!(sent["fields"]["issuetype"]["name"], "Task");
        assert_eq!(sent["fields"]["labels"][0], merge0_signal::ORIGIN_LABEL);
        assert_eq!(sent["fields"]["description"]["type"], "doc");
        assert_eq!(
            sent["fields"]["description"]["content"][0]["content"][0]["text"],
            "Line one"
        );
    }

    #[tokio::test]
    async fn jira_rejection_carries_the_reason_not_just_a_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"errors":{"project":"project 'NOPE' does not exist"}}"#),
            )
            .mount(&server)
            .await;

        let tracker =
            JiraTracker::new(server.uri(), "bot@example.com", "t", "NOPE", "Task").unwrap();
        let err = tracker.create_story(&story()).await.expect_err("must fail");

        match err {
            TrackerError::Api { status, body } => {
                assert_eq!(status, 400);
                assert!(
                    body.contains("does not exist"),
                    "reason must survive: {body}"
                );
            }
            other => panic!("expected an Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_response_without_a_key_is_malformed_not_a_silent_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "10042" })))
            .mount(&server)
            .await;

        let tracker =
            JiraTracker::new(server.uri(), "bot@example.com", "t", "ENG", "Task").unwrap();
        let err = tracker.create_story(&story()).await.expect_err("must fail");
        assert!(matches!(err, TrackerError::Malformed(_)));
    }

    #[test]
    fn adf_makes_one_paragraph_per_line_and_drops_blanks() {
        let doc = to_adf("first\n\nsecond\n");
        assert_eq!(doc["version"], 1);
        let content = doc["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "blank line must not become a paragraph");
        assert_eq!(content[1]["content"][0]["text"], "second");
    }
}
