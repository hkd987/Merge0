//! Runner (PRD §5): BYO agent, BYO compute.
//!
//! Merge0 dispatches an approved, sanitized Work Order to the *customer's*
//! GitHub Actions via `repository_dispatch`; the customer-side workflow
//! (template in [`workflow`]) clones, runs the agent with the customer's own
//! key, enforces the repair and diff budgets, and reports back. Merge0 never
//! clones customer code and never sees a runner credential.
//!
//! The interface is agent-agnostic ([`AgentKind`]) — alternatives are a
//! config change, not a rewrite (P2 requirement designed in now).

use async_trait::async_trait;
use merge0_github::{GitHubApi, GitHubError, RepoRef};
use merge0_signal::WorkOrder;
use serde::{Deserialize, Serialize};

pub mod report;
pub mod workflow;

pub use report::{enforce_budgets, RunReport, RunStatus};

/// The event type the customer workflow subscribes to.
pub const DISPATCH_EVENT: &str = "merge0-work-order";

/// Default number of test-repair iterations before self-discard (PRD §5).
pub const DEFAULT_REPAIR_BUDGET: u32 = 3;

/// Which coding agent the customer workflow runs (PRD P2: agent-agnostic
/// runner configs — the interface exists now, `ClaudeCode` is v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "command")]
pub enum AgentKind {
    /// Headless Claude Code (`claude -p`).
    ClaudeCode,
    /// Codex CLI.
    CodexCli,
    /// Any other headless agent; the command receives the work-order file
    /// path as `$MERGE0_WORK_ORDER`.
    Custom(String),
}

impl AgentKind {
    pub fn label(&self) -> &str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::CodexCli => "codex-cli",
            AgentKind::Custom(_) => "custom",
        }
    }

    /// The shell command the workflow template embeds.
    pub fn command(&self) -> String {
        match self {
            AgentKind::ClaudeCode => {
                "claude -p \"$(cat \"$MERGE0_WORK_ORDER\")\" --allowedTools \"Edit,Write,Bash(git *),Bash(cargo *),Bash(npm *),Bash(npx *)\"".to_string()
            }
            AgentKind::CodexCli => "codex exec \"$(cat \"$MERGE0_WORK_ORDER\")\"".to_string(),
            AgentKind::Custom(command) => command.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] GitHubError),
    #[error("work order failed sanitization: {0}")]
    Sanitization(String),
}

/// What dispatch hands back for tracking.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchReceipt {
    pub repo: RepoRef,
    pub runner_kind: String,
}

#[async_trait]
pub trait Runner: Send + Sync {
    async fn dispatch(&self, order: &WorkOrder) -> Result<DispatchReceipt, RunnerError>;
}

/// The v1 runner: `repository_dispatch` into the customer's Actions.
pub struct ActionsRunner<A> {
    pub api: A,
    pub agent: AgentKind,
    /// Where the customer workflow calls back with its RunReport.
    pub callback_url: String,
}

#[async_trait]
impl<A: GitHubApi> Runner for ActionsRunner<A> {
    async fn dispatch(&self, order: &WorkOrder) -> Result<DispatchReceipt, RunnerError> {
        let repo = RepoRef::parse(&order.repo)
            .map_err(|e| RunnerError::Sanitization(format!("bad repo: {e}")))?;
        let payload = sanitized_payload(order, &self.callback_url)?;
        self.api
            .repository_dispatch(&repo, DISPATCH_EVENT, &payload)
            .await?;
        Ok(DispatchReceipt {
            repo,
            runner_kind: self.agent.label().to_string(),
        })
    }
}

/// Build the dispatch payload. Sanitization invariants (PRD §5 Secrets):
/// no raw vendor payloads, no credentials, evidence restricted to URLs and
/// labels — enforced here because the payload crosses into customer CI logs.
pub fn sanitized_payload(
    order: &WorkOrder,
    callback_url: &str,
) -> Result<serde_json::Value, RunnerError> {
    let value = serde_json::to_value(order).expect("work order serializes");
    // Defense in depth: WorkOrder has no `raw` field by construction, but a
    // future refactor must not silently start leaking one.
    if value.get("raw").is_some() {
        return Err(RunnerError::Sanitization(
            "work order must not carry raw vendor payloads".into(),
        ));
    }
    let text = value.to_string();
    for marker in ["api_key", "apikey", "secret", "password", "authorization"] {
        if text.to_lowercase().contains(marker) {
            return Err(RunnerError::Sanitization(format!(
                "work order text contains credential marker {marker:?}"
            )));
        }
    }
    Ok(serde_json::json!({
        "work_order": value,
        "callback_url": callback_url,
        "repair_budget": DEFAULT_REPAIR_BUDGET,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_github::FakeGitHub;
    use merge0_signal::DiffBudget;
    use ulid::Ulid;

    fn order() -> WorkOrder {
        WorkOrder {
            report_id: Ulid::new(),
            repo: "chalk/chalk".into(),
            summary: "Fix crash".into(),
            evidence: vec![],
            repro: "open the page".into(),
            suspect_change: None,
            success_criteria: "test passes".into(),
            constraints: String::new(),
            prior_attempts: vec![],
            diff_budget: DiffBudget::default(),
        }
    }

    #[tokio::test]
    async fn dispatch_sends_sanitized_payload_with_budgets() {
        let api = FakeGitHub::new();
        let runner = ActionsRunner {
            api,
            agent: AgentKind::ClaudeCode,
            callback_url: "https://merge0.example.com/runner/callback".into(),
        };
        let receipt = runner.dispatch(&order()).await.unwrap();
        assert_eq!(receipt.runner_kind, "claude-code");

        let state = runner.api.state.lock().unwrap();
        let (repo, event, payload) = &state.dispatches[0];
        assert_eq!(repo.full(), "chalk/chalk");
        assert_eq!(event, DISPATCH_EVENT);
        assert_eq!(payload["repair_budget"], 3);
        assert_eq!(payload["work_order"]["diff_budget"]["max_files"], 4);
        assert_eq!(
            payload["callback_url"],
            "https://merge0.example.com/runner/callback"
        );
    }

    #[tokio::test]
    async fn credential_markers_fail_sanitization() {
        let mut bad = order();
        bad.constraints = "use the API_KEY=sk-123 from env".into();
        let api = FakeGitHub::new();
        let runner = ActionsRunner {
            api,
            agent: AgentKind::ClaudeCode,
            callback_url: "https://cb".into(),
        };
        assert!(matches!(
            runner.dispatch(&bad).await,
            Err(RunnerError::Sanitization(_))
        ));
        assert!(runner.api.state.lock().unwrap().dispatches.is_empty());
    }

    #[tokio::test]
    async fn bad_repo_never_reaches_the_api() {
        let mut bad = order();
        bad.repo = "not-a-repo".into();
        let api = FakeGitHub::new();
        let runner = ActionsRunner {
            api,
            agent: AgentKind::ClaudeCode,
            callback_url: "https://cb".into(),
        };
        assert!(runner.dispatch(&bad).await.is_err());
        assert!(runner.api.state.lock().unwrap().dispatches.is_empty());
    }

    #[test]
    fn agent_kinds_have_commands() {
        assert!(AgentKind::ClaudeCode.command().contains("claude -p"));
        assert!(AgentKind::CodexCli.command().contains("codex"));
        assert_eq!(AgentKind::Custom("./run.sh".into()).command(), "./run.sh");
        assert_eq!(
            serde_json::to_value(AgentKind::ClaudeCode).unwrap()["kind"],
            "claude_code"
        );
    }
}
