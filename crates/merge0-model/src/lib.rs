//! Model abstraction for triage.
//!
//! The gate and the clustering pass talk to a [`Model`]; production uses
//! [`AnthropicModel`] with the customer's own key (BYO agent extends to BYO
//! triage model), tests use [`ScriptedModel`]. Base prompts are config
//! (`config/`), never modified at runtime (PRD §5d).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    pub text: String,
    /// Input + output tokens, for cost accounting.
    pub tokens_used: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(String),
    #[error("model returned an unusable response: {0}")]
    BadResponse(String),
    #[error("scripted model ran out of responses (got {0} requests)")]
    ScriptExhausted(usize),
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError>;
}

/// Extract the first top-level JSON object from model text — models wrap
/// JSON in prose and code fences; downstream parsers should not care.
pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---- Anthropic client ----

/// Messages API client. The key is provided at construction by the caller
/// (resolved from the customer's environment) and never logged; `Debug`
/// redacts it.
pub struct AnthropicModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model_id: String,
}

impl std::fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicModel")
            .field("model_id", &self.model_id)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl AnthropicModel {
    pub fn new(api_key: String, model_id: String) -> Self {
        AnthropicModel {
            client: reqwest::Client::new(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key,
            model_id,
        }
    }

    /// Override the endpoint (tests, proxies).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// The request payload, exposed as a pure function so serialization is
    /// testable without network.
    pub fn payload(&self, request: &ModelRequest) -> serde_json::Value {
        serde_json::json!({
            "model": self.model_id,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": [{ "role": "user", "content": request.prompt }],
        })
    }
}

#[async_trait]
impl Model for AnthropicModel {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.payload(request))
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ModelError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            return Err(ModelError::Transport(format!(
                "API returned {status}: {}",
                body["error"]["message"].as_str().unwrap_or("unknown")
            )));
        }
        let text = body["content"][0]["text"]
            .as_str()
            .ok_or_else(|| ModelError::BadResponse("no text content block".into()))?
            .to_string();
        let tokens_used = body["usage"]["input_tokens"].as_u64().unwrap_or(0)
            + body["usage"]["output_tokens"].as_u64().unwrap_or(0);
        Ok(ModelResponse { text, tokens_used })
    }
}

// ---- Scripted fake ----

/// Deterministic model for tests: returns queued responses in order and
/// records every request for assertion.
#[derive(Default)]
pub struct ScriptedModel {
    responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    requests: std::sync::Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub fn new(responses: impl IntoIterator<Item = impl Into<String>>) -> Self {
        ScriptedModel {
            responses: std::sync::Mutex::new(responses.into_iter().map(Into::into).collect()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request.clone());
        let count = requests.len();
        drop(requests);
        let text = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ModelError::ScriptExhausted(count))?;
        Ok(ModelResponse {
            text,
            tokens_used: 1000,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_handles_fences_prose_and_nesting() {
        let text = "Here is my decision:\n```json\n{\"decision\":\"work\",\"nested\":{\"a\":\"}\"}}\n```\ndone";
        let json = extract_json_object(text).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["decision"], "work");
        assert_eq!(value["nested"]["a"], "}");
        assert_eq!(extract_json_object("no json here"), None);
        assert_eq!(extract_json_object("{\"unterminated\": true"), None);
    }

    #[test]
    fn anthropic_payload_shape() {
        let model = AnthropicModel::new("test-key".into(), "claude-haiku-4-5-20251001".into());
        let payload = model.payload(&ModelRequest {
            system: "you are the gate".into(),
            prompt: "report...".into(),
            max_tokens: 1024,
        });
        assert_eq!(payload["model"], "claude-haiku-4-5-20251001");
        assert_eq!(payload["system"], "you are the gate");
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["max_tokens"], 1024);
    }

    #[test]
    fn anthropic_debug_redacts_key() {
        let model = AnthropicModel::new("sk-secret".into(), "m".into());
        let debug = format!("{model:?}");
        assert!(!debug.contains("sk-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn scripted_model_returns_in_order_then_errors() {
        let model = ScriptedModel::new(["one", "two"]);
        let request = ModelRequest {
            system: String::new(),
            prompt: "p".into(),
            max_tokens: 10,
        };
        assert_eq!(model.complete(&request).await.unwrap().text, "one");
        assert_eq!(model.complete(&request).await.unwrap().text, "two");
        assert!(matches!(
            model.complete(&request).await,
            Err(ModelError::ScriptExhausted(3))
        ));
        assert_eq!(model.requests().len(), 3);
    }
}
