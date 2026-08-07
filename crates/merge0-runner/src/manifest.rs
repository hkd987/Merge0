//! `.merge0/agent.toml` — the customer's agent-extensibility manifest
//! (PRD §5b). Governing rule enforced here: **configuration transits
//! Merge0; credentials never do** — `auth_env` entries are validated to be
//! environment-variable *names*, never values.
//!
//! The manifest lives in the customer repo and changes only through their
//! PR review; Merge0 fetches and parses it at dispatch time to generate the
//! workflow (MCP wiring, egress allowlist, test command) and to record
//! per-run extension attribution into outcome memory.

use serde::Deserialize;

pub const MANIFEST_PATH: &str = ".merge0/agent.toml";

/// Default test entrypoint when the manifest omits `[test]` — documented in
/// the onboarding checklist as the file the customer must provide.
pub const DEFAULT_TEST_COMMAND: &str = "./merge0-test.sh";

/// Shipped template served by the onboarding endpoint.
pub const AGENT_TOML_TEMPLATE: &str = r#"# .merge0/agent.toml — Merge0 runner manifest (PRD §5b).
# Configuration transits Merge0; credentials never do: `auth_env` is the
# NAME of a secret in your CI environment, resolved inside your job.

# REQUIRED: how the runner verifies a fix. Must exit non-zero on failure.
[test]
command = "./merge0-test.sh"

# Optional: MCP servers the agent may use.
# [[mcp]]
# name = "internal-api"
# command = "npx our-api-mcp"
# auth_env = "INTERNAL_API_KEY"

# Optional: git-native skills directory (reviewed via your normal PRs).
# [skills]
# path = ".merge0/skills/"

# Optional: egress allowlist enforced in the runner job. When present,
# outbound traffic is restricted to these hosts plus the GitHub/Anthropic
# endpoints the job itself needs.
# [network]
# egress_allow = ["api.internal.example.com"]
"#;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ManifestError {
    #[error("invalid agent.toml: {0}")]
    Parse(String),
    #[error("auth_env {0:?} is not an environment variable name (uppercase letters, digits, underscores)")]
    BadAuthEnv(String),
    #[error("mcp entry {0:?} has an empty command")]
    EmptyCommand(String),
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    #[serde(default, rename = "mcp")]
    pub mcps: Vec<McpDecl>,
    #[serde(default)]
    pub skills: Option<SkillsDecl>,
    #[serde(default)]
    pub network: Option<NetworkDecl>,
    #[serde(default)]
    pub test: Option<TestDecl>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDecl {
    pub name: String,
    pub command: String,
    /// Secret reference BY NAME — resolved from the customer's own CI
    /// secrets inside the job; the value never transits Merge0.
    #[serde(default)]
    pub auth_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsDecl {
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDecl {
    #[serde(default)]
    pub egress_allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestDecl {
    pub command: String,
}

impl AgentManifest {
    pub fn parse(toml_text: &str) -> Result<AgentManifest, ManifestError> {
        let manifest: AgentManifest =
            toml::from_str(toml_text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        for mcp in &manifest.mcps {
            if mcp.command.trim().is_empty() {
                return Err(ManifestError::EmptyCommand(mcp.name.clone()));
            }
            if let Some(auth_env) = &mcp.auth_env {
                let valid_name = !auth_env.is_empty()
                    && auth_env
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if !valid_name {
                    // A value (or anything shaped like one) instead of a
                    // name is exactly the credential-transit mistake §5b
                    // forbids — reject loudly.
                    return Err(ManifestError::BadAuthEnv(auth_env.clone()));
                }
            }
        }
        Ok(manifest)
    }

    pub fn test_command(&self) -> &str {
        self.test
            .as_ref()
            .map(|t| t.command.as_str())
            .unwrap_or(DEFAULT_TEST_COMMAND)
    }

    pub fn egress_allow(&self) -> &[String] {
        self.network
            .as_ref()
            .map(|n| n.egress_allow.as_slice())
            .unwrap_or(&[])
    }

    /// The names of the CI secrets the workflow must map into the job env.
    pub fn auth_env_names(&self) -> Vec<&str> {
        self.mcps
            .iter()
            .filter_map(|m| m.auth_env.as_deref())
            .collect()
    }

    /// Extension attribution recorded into outcome memory at dispatch
    /// (PRD §5b: extension quality is measurable).
    pub fn attribution(&self) -> serde_json::Value {
        serde_json::json!({
            "mcp": self.mcps.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
            "skills_path": self.skills.as_ref().map(|s| s.path.clone()),
            "egress_allow": self.egress_allow(),
            "test_command": self.test_command(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_defaults_apply() {
        let manifest = AgentManifest::parse(AGENT_TOML_TEMPLATE).unwrap();
        assert_eq!(manifest.test_command(), "./merge0-test.sh");
        assert!(manifest.mcps.is_empty());
        assert!(manifest.egress_allow().is_empty());
        let empty = AgentManifest::parse("").unwrap();
        assert_eq!(empty.test_command(), DEFAULT_TEST_COMMAND);
    }

    #[test]
    fn full_manifest_round_trips() {
        let manifest = AgentManifest::parse(
            r#"
            [[mcp]]
            name = "internal-api"
            command = "npx our-api-mcp"
            auth_env = "INTERNAL_API_KEY"

            [skills]
            path = ".merge0/skills/"

            [network]
            egress_allow = ["api.internal.example.com"]

            [test]
            command = "cargo test --workspace"
            "#,
        )
        .unwrap();
        assert_eq!(manifest.test_command(), "cargo test --workspace");
        assert_eq!(manifest.auth_env_names(), vec!["INTERNAL_API_KEY"]);
        assert_eq!(manifest.egress_allow(), ["api.internal.example.com"]);
        let attribution = manifest.attribution();
        assert_eq!(attribution["mcp"][0], "internal-api");
        assert_eq!(attribution["skills_path"], ".merge0/skills/");
    }

    #[test]
    fn credential_values_masquerading_as_names_are_rejected() {
        for bad in ["sk-ant-secret", "KEY=value", "lower_case", "", "HAS SPACE"] {
            let toml = format!("[[mcp]]\nname = \"x\"\ncommand = \"c\"\nauth_env = \"{bad}\"\n");
            assert!(
                matches!(
                    AgentManifest::parse(&toml),
                    Err(ManifestError::BadAuthEnv(_))
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn unknown_fields_and_empty_commands_rejected() {
        assert!(matches!(
            AgentManifest::parse("[[mcp]]\nname = \"x\"\ncommand = \"  \"\n"),
            Err(ManifestError::EmptyCommand(_))
        ));
        assert!(matches!(
            AgentManifest::parse("[test]\ncommand = \"t\"\nunknown_key = 1\n"),
            Err(ManifestError::Parse(_))
        ));
    }
}
