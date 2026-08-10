//! `Model` backed by the Claude Code CLI: `claude -p --output-format json`
//! with tools disabled — a pure single-turn completion using whatever auth
//! the operator's CLI already holds. The binary is configurable so tests
//! can substitute a stub script.

use crate::{Model, ModelError, ModelRequest, ModelResponse};
use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct CliModel {
    binary: String,
    /// Optional `--model` passthrough (e.g. a haiku id for cheap dry runs).
    model: Option<String>,
}

/// The CLI's `--output-format json` envelope (the fields we consume).
#[derive(Debug, Deserialize)]
struct CliEnvelope {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    usage: Option<CliUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl CliModel {
    /// Binary from `MERGE0_EVAL_CLI` (default `claude`), model override from
    /// `MERGE0_EVAL_MODEL` (default: the CLI's configured model).
    pub fn from_env() -> CliModel {
        CliModel {
            binary: std::env::var("MERGE0_EVAL_CLI").unwrap_or_else(|_| "claude".into()),
            model: std::env::var("MERGE0_EVAL_MODEL").ok(),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> CliModel {
        CliModel {
            binary: binary.into(),
            model: None,
        }
    }

    /// Override the `--model` passthrough (None keeps the CLI's default).
    pub fn model(mut self, model: Option<String>) -> CliModel {
        self.model = model;
        self
    }
}

#[async_trait]
impl Model for CliModel {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let mut command = Command::new(&self.binary);
        command
            // Print mode, prompt on stdin (argv limits are a real hazard
            // for file-sized inputs).
            .arg("-p")
            .args(["--output-format", "json"])
            // A completion, not an agent: no tools, no side effects.
            .args(["--tools", ""])
            .args(["--system-prompt", &request.system])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(model) = &self.model {
            command.args(["--model", model]);
        }
        let mut child = command
            .spawn()
            .map_err(|e| ModelError::Transport(format!("spawn {}: {e}", self.binary)))?;
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(request.prompt.as_bytes())
            .await
            .map_err(|e| ModelError::Transport(format!("write prompt: {e}")))?;
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ModelError::Transport(format!("wait for {}: {e}", self.binary)))?;
        if !output.status.success() {
            return Err(ModelError::Transport(format!(
                "{} exited with {}: {}",
                self.binary,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let envelope: CliEnvelope = serde_json::from_slice(&output.stdout).map_err(|e| {
            ModelError::BadResponse(format!(
                "CLI output is not the json envelope ({e}): {}",
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(300)
                    .collect::<String>()
            ))
        })?;
        if envelope.is_error {
            return Err(ModelError::BadResponse(format!(
                "CLI reported is_error: {}",
                envelope.result.as_deref().unwrap_or("no result text")
            )));
        }
        let usage = envelope.usage.unwrap_or_default();
        Ok(ModelResponse {
            text: envelope
                .result
                .ok_or_else(|| ModelError::BadResponse("envelope has no result".into()))?,
            tokens_used: usage.input_tokens + usage.output_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write an executable stub standing in for the `claude` binary, and
    /// return only once it can actually be exec'd.
    ///
    /// The wait is not superstition: tests in one binary run on many
    /// threads, and `fork` from any of them duplicates every open write fd
    /// in the process. If a sibling thread forks while this file is still
    /// open for writing, the kernel refuses our `exec` with `ETXTBSY`
    /// ("Text file busy") until that child execs and drops the inherited
    /// descriptor. The window is microseconds, but it is real — it failed a
    /// full-workspace run. Probing until exec succeeds turns a flake into a
    /// deterministic wait; the stubs are side-effect-free, so the extra
    /// invocation costs nothing.
    fn stub(body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("merge0-cli-stub-{}", ulid::Ulid::generate()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        for attempt in 0..200 {
            match std::process::Command::new(&path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    return path;
                }
                // 26 == ETXTBSY. Anything else is a genuine failure.
                Err(e) if e.raw_os_error() == Some(26) && attempt < 199 => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("stub {} is not executable: {e}", path.display()),
            }
        }
        unreachable!("loop returns or panics")
    }

    fn request() -> ModelRequest {
        ModelRequest {
            system: "you are the gate".into(),
            prompt: "REPORT ...".into(),
            max_tokens: 2048,
        }
    }

    #[tokio::test]
    async fn parses_result_and_tokens_from_the_envelope() {
        let path = stub(
            r#"cat > /dev/null
echo '{"result":"{\"decision\":\"skip\",\"reason\":\"stub\"}","is_error":false,"usage":{"input_tokens":10,"output_tokens":5}}'"#,
        );
        let model = CliModel::with_binary(path.to_str().unwrap());
        let response = model.complete(&request()).await.unwrap();
        assert!(response.text.contains(r#""decision":"skip""#));
        assert_eq!(response.tokens_used, 15);
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn cli_error_envelope_is_a_bad_response() {
        let path = stub(
            r#"cat > /dev/null
echo '{"result":"over quota","is_error":true}'"#,
        );
        let model = CliModel::with_binary(path.to_str().unwrap());
        let err = model.complete(&request()).await.unwrap_err();
        assert!(matches!(err, ModelError::BadResponse(_)), "{err}");
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_transport_error() {
        let path = stub("cat > /dev/null\nexit 3");
        let model = CliModel::with_binary(path.to_str().unwrap());
        let err = model.complete(&request()).await.unwrap_err();
        assert!(matches!(err, ModelError::Transport(_)), "{err}");
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn missing_binary_is_a_transport_error() {
        let model = CliModel::with_binary("/nonexistent/merge0-no-such-cli");
        let err = model.complete(&request()).await.unwrap_err();
        assert!(matches!(err, ModelError::Transport(_)), "{err}");
    }
}
