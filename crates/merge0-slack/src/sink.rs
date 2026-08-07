//! Message delivery, abstracted behind [`SlackSink`].
//!
//! Builders stay pure; the sink is the only I/O boundary. [`WebhookSink`]
//! posts to a Slack incoming webhook; [`RecordingSink`] captures messages
//! in memory for tests and dry-run mode, so the server can exercise the
//! full notification path without a network.

use crate::SlackError;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Mutex, PoisonError};

/// Anything that can deliver a Block Kit message.
#[async_trait]
pub trait SlackSink: Send + Sync {
    async fn post(&self, message: &Value) -> Result<(), SlackError>;
}

/// Posts messages to a Slack incoming-webhook URL.
pub struct WebhookSink {
    url: String,
    client: reqwest::Client,
}

impl WebhookSink {
    pub fn new(webhook_url: impl Into<String>) -> Self {
        Self {
            url: webhook_url.into(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client with static configuration"),
        }
    }
}

#[async_trait]
impl SlackSink for WebhookSink {
    async fn post(&self, message: &Value) -> Result<(), SlackError> {
        let response = self.client.post(&self.url).json(message).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(SlackError::WebhookStatus(status.as_u16()));
        }
        Ok(())
    }
}

/// Records every posted message in memory (tests, dry-run).
#[derive(Default)]
pub struct RecordingSink {
    messages: Mutex<Vec<Value>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of everything posted so far, in order.
    pub fn recorded(&self) -> Vec<Value> {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl SlackSink for RecordingSink {
    async fn post(&self, message: &Value) -> Result<(), SlackError> {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn recording_sink_captures_messages_in_order() {
        let sink = RecordingSink::new();
        sink.post(&json!({ "text": "first" })).await.unwrap();
        sink.post(&json!({ "text": "second" })).await.unwrap();
        let recorded = sink.recorded();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0]["text"], "first");
        assert_eq!(recorded[1]["text"], "second");
    }

    #[tokio::test]
    async fn recording_sink_works_through_the_trait_object() {
        let sink: Box<dyn SlackSink> = Box::new(RecordingSink::new());
        sink.post(&json!({ "text": "via trait" })).await.unwrap();
    }
}
