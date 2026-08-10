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

pub mod manifest;
pub mod report;
pub mod workflow;

pub use manifest::{AgentManifest, ManifestError};
pub use report::{enforce_budgets, RunReport, RunStatus};

/// The event type the customer workflow subscribes to.
pub const DISPATCH_EVENT: &str = "merge0-work-order";

/// Default number of test-repair iterations before self-discard (PRD §5).
pub const DEFAULT_REPAIR_BUDGET: u32 = 3;

/// Which coding agent the customer workflow runs (PRD P2: agent-agnostic
/// runner configs). Every preset invokes the harness's documented headless
/// mode with the sanitized work-order JSON as the entire prompt; the run
/// executes in the customer's own ephemeral CI job with a job-scoped token,
/// which is the actual security boundary — per-CLI sandbox flags are
/// defense in depth on top of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "command")]
pub enum AgentKind {
    /// Headless Claude Code (`claude -p`), with a scoped tool allowlist.
    ClaudeCode,
    /// OpenAI Codex CLI (`codex exec`), workspace-write sandbox.
    CodexCli,
    /// Google Gemini CLI (`gemini -p`), yolo approval (CI-trusted).
    GeminiCli,
    /// Aider (`--message` single-pass). Auto-commits are disabled because
    /// the workflow measures the diff against the base commit and makes
    /// the commit itself — an agent that commits breaks both.
    Aider,
    /// OpenCode (`opencode run`). Model comes from the customer's own
    /// OpenCode config or the `OPENCODE_MODEL` repo variable.
    Opencode,
    /// Cursor CLI (`cursor-agent -p`), print mode.
    CursorCli,
    /// Any other headless agent; the command receives the work-order file
    /// path as `$MERGE0_WORK_ORDER`.
    Custom(String),
}

/// Labels accepted by `MERGE0_AGENT`, kept in sync with [`AgentKind::label`].
pub const AGENT_LABELS: &[&str] = &[
    "claude-code",
    "codex-cli",
    "gemini-cli",
    "aider",
    "opencode",
    "cursor-cli",
];

impl AgentKind {
    pub fn label(&self) -> &str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::CodexCli => "codex-cli",
            AgentKind::GeminiCli => "gemini-cli",
            AgentKind::Aider => "aider",
            AgentKind::Opencode => "opencode",
            AgentKind::CursorCli => "cursor-cli",
            AgentKind::Custom(_) => "custom",
        }
    }

    /// Parse the `MERGE0_AGENT` env value. `None`/empty means the default
    /// (Claude Code); an unrecognized value is an ERROR, not a silent
    /// fallback — a typo'd agent must fail startup loudly, never dispatch
    /// work to a different harness than the operator intended.
    pub fn from_env_value(value: Option<&str>) -> Result<AgentKind, String> {
        match value.map(str::trim) {
            None | Some("") | Some("claude-code") => Ok(AgentKind::ClaudeCode),
            Some("codex-cli") => Ok(AgentKind::CodexCli),
            Some("gemini-cli") => Ok(AgentKind::GeminiCli),
            Some("aider") => Ok(AgentKind::Aider),
            Some("opencode") => Ok(AgentKind::Opencode),
            Some("cursor-cli") => Ok(AgentKind::CursorCli),
            Some(custom) if custom.starts_with("custom:") => {
                let command = custom.trim_start_matches("custom:").trim();
                if command.is_empty() {
                    return Err("MERGE0_AGENT=custom: requires a command after the colon".into());
                }
                Ok(AgentKind::Custom(command.to_string()))
            }
            Some(other) => Err(format!(
                "unknown MERGE0_AGENT {other:?}; expected one of {AGENT_LABELS:?} or custom:<command>"
            )),
        }
    }

    /// The shell command the workflow template embeds. Each preset is the
    /// harness's documented non-interactive form.
    pub fn command(&self) -> String {
        const PROMPT: &str = "\"$(cat \"$MERGE0_WORK_ORDER\")\"";
        match self {
            AgentKind::ClaudeCode => format!(
                "claude -p {PROMPT} --allowedTools \"Edit,Write,Bash(git *),Bash(cargo *),Bash(npm *)\""
            ),
            AgentKind::CodexCli => {
                format!("codex exec --sandbox workspace-write {PROMPT}")
            }
            AgentKind::GeminiCli => format!("gemini -p {PROMPT} --approval-mode=yolo"),
            AgentKind::Aider => {
                format!("aider --message {PROMPT} --yes-always --no-auto-commits")
            }
            AgentKind::Opencode => format!("opencode run {PROMPT}"),
            AgentKind::CursorCli => format!("cursor-agent -p {PROMPT}"),
            AgentKind::Custom(command) => command.clone(),
        }
    }

    /// The CI secret NAMES the generated workflow maps into the agent
    /// step's environment — resolved from the customer repo's own Actions
    /// secrets; values never transit Merge0. Multi-provider harnesses map
    /// both common keys, and the subscription-capable harnesses also map
    /// their subscription credential (Claude Pro/Max via
    /// `claude setup-token` → `CLAUDE_CODE_OAUTH_TOKEN`; ChatGPT via the
    /// contents of `~/.codex/auth.json` → `CODEX_AUTH_JSON`). Unset
    /// secrets resolve to empty and are unset again by the workflow's
    /// auth preamble, so configuring EITHER credential is enough.
    pub fn auth_env_names(&self) -> &'static [&'static str] {
        match self {
            AgentKind::ClaudeCode => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
            AgentKind::CodexCli => &["OPENAI_API_KEY", "CODEX_AUTH_JSON"],
            AgentKind::GeminiCli => &["GEMINI_API_KEY"],
            AgentKind::Aider | AgentKind::Opencode => &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
            AgentKind::CursorCli => &["CURSOR_API_KEY"],
            // Unknown harness: map the common pair so most custom commands
            // work; anything else is added via the manifest's auth_env.
            AgentKind::Custom(_) => &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        }
    }

    /// Model-provider hosts the egress allowlist must keep reachable for
    /// this agent (joined with the GitHub infra hosts in the workflow).
    /// Subscription auth changes where inference goes: Claude Code's OAuth
    /// tokens exchange against claude.ai, and Codex under ChatGPT sign-in
    /// talks to chatgpt.com (not api.openai.com) — omitting those holes
    /// would make subscription mode fail only under an egress allowlist,
    /// the worst kind of works-on-my-machine.
    pub fn api_hosts(&self) -> &'static [&'static str] {
        match self {
            AgentKind::ClaudeCode => &["api.anthropic.com", "claude.ai"],
            AgentKind::CodexCli => &["api.openai.com", "chatgpt.com", "auth.openai.com"],
            AgentKind::GeminiCli => &["generativelanguage.googleapis.com"],
            AgentKind::Aider | AgentKind::Opencode => &["api.anthropic.com", "api.openai.com"],
            AgentKind::CursorCli => &["api.cursor.com"],
            AgentKind::Custom(_) => &["api.anthropic.com", "api.openai.com"],
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
    /// Manifest attribution (PRD §5b) echoed back by the workflow so
    /// per-run extension usage lands in outcome memory. `None` when the
    /// repo has no manifest.
    pub attribution: Option<serde_json::Value>,
}

#[async_trait]
impl<A: GitHubApi> Runner for ActionsRunner<A> {
    async fn dispatch(&self, order: &WorkOrder) -> Result<DispatchReceipt, RunnerError> {
        let repo = RepoRef::parse(&order.repo)
            .map_err(|e| RunnerError::Sanitization(format!("bad repo: {e}")))?;
        let payload = sanitized_payload(order, &self.callback_url, self.attribution.as_ref())?;
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
/// (The attribution block carries manifest *names* only — the manifest
/// parser already rejects anything shaped like a credential value.)
pub fn sanitized_payload(
    order: &WorkOrder,
    callback_url: &str,
    attribution: Option<&serde_json::Value>,
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
        "attribution": attribution,
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
            report_id: Ulid::generate(),
            repo: "chalk/chalk".into(),
            summary: "Fix crash".into(),
            evidence: vec![],
            repro: "open the page".into(),
            suspect_change: None,
            success_criteria: "test passes".into(),
            constraints: String::new(),
            prior_attempts: vec![],
            diff_budget: DiffBudget::default(),
            confidence: Default::default(),
        }
    }

    #[tokio::test]
    async fn dispatch_sends_sanitized_payload_with_budgets() {
        let api = FakeGitHub::new();
        let runner = ActionsRunner {
            api,
            agent: AgentKind::ClaudeCode,
            callback_url: "https://merge0.example.com/runner/callback".into(),
            attribution: Some(serde_json::json!({"mcp": ["internal-api"]})),
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
            attribution: None,
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
            attribution: None,
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

    #[test]
    fn every_label_parses_back_to_its_kind_and_typos_fail_loudly() {
        for label in AGENT_LABELS {
            let kind = AgentKind::from_env_value(Some(label)).unwrap();
            assert_eq!(kind.label(), *label, "label round-trip");
            assert!(!kind.auth_env_names().is_empty());
            assert!(!kind.api_hosts().is_empty());
        }
        assert_eq!(
            AgentKind::from_env_value(None).unwrap(),
            AgentKind::ClaudeCode
        );
        assert_eq!(
            AgentKind::from_env_value(Some("custom:./agent.sh")).unwrap(),
            AgentKind::Custom("./agent.sh".into())
        );
        // A typo must be a startup error, never a silent fallback to a
        // different harness than the operator intended.
        for bad in ["claud-code", "codex", "gemini", "custom:"] {
            assert!(
                AgentKind::from_env_value(Some(bad)).is_err(),
                "{bad:?} accepted"
            );
        }
    }
}
