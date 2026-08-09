//! Typed loading of scout/gate configuration.
//!
//! Config drift breaks CI, not runtime: tests in this module load the actual
//! files under `config/` at the repo root.

use merge0_signal::{GateConfidence, Severity, Source};
use serde::Deserialize;
use std::path::Path;

/// A scout is a standing question: a prompt plus a source query template,
/// scheduled — defined entirely as config (PRD §4).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoutConfig {
    pub name: String,
    pub description: String,
    /// Cron-ish schedule label; the scheduler interprets it ("nightly" for
    /// Phase 0).
    pub schedule: String,
    /// Which sources this scout queries.
    pub sources: Vec<Source>,
    /// Executed filter over the Signal store (see [`crate::query`] for the
    /// grammar). Empty or `"*"` selects everything in the window.
    pub query_template: String,
    /// The standing question posed to the scout model.
    pub prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// The gate's quality bar (PRD §4): conservative by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    /// The gate prompt evaluated per Report against intent context and
    /// outcome memory. Outputs a Work Order or SKIP with reason.
    pub prompt: String,
    /// Reports below this severity are skipped without a model call.
    pub min_severity: Severity,
    /// Hard cap per triage run — a quiet inbox that's right beats a busy one.
    pub max_work_orders_per_run: u32,
    /// Evidence budget: max links assembled onto a Report/Work Order.
    #[serde(default = "default_max_evidence_items")]
    pub max_evidence_items: usize,
    /// Evidence budget: character cap per assembled text section.
    #[serde(default = "default_max_section_chars")]
    pub max_section_chars: usize,
    /// Diff budget stamped on emitted Work Orders (PRD §5).
    #[serde(default = "default_max_files")]
    pub diff_max_files: u32,
    #[serde(default = "default_max_total_lines")]
    pub diff_max_total_lines: u32,
    /// Cap on prior attempts assembled from outcome memory.
    #[serde(default = "default_prior_attempts_cap")]
    pub prior_attempts_cap: usize,
    /// Age past which a prior attempt is rendered to the gate as STALE.
    /// Memory without recency is memory that over-vetoes: a fix that failed
    /// once two years ago should inform the decision, not forbid it.
    #[serde(default = "default_stale_prior_days")]
    pub stale_prior_days: u32,
    /// The autonomy dial (off by default): auto-dispatch of high-confidence
    /// Work Orders without a human click.
    #[serde(default)]
    pub autonomy: AutonomyConfig,
    /// Hard model-spend ceiling (0 = unlimited).
    #[serde(default)]
    pub budget: BudgetConfig,
    /// What a Work Order's own confidence changes about how it is delivered.
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// Confidence routing. The gate already tells us how sure it is; before
/// this, nothing acted on the answer — a low-confidence Work Order became
/// an autonomous PR exactly like a high-confidence one, and the uncertainty
/// was resolved by whichever way the model happened to fall that run.
///
/// Routing turns that coin-flip into a product decision: below the floor,
/// the same evidence-backed Work Order is filed as a tracker story for a
/// human instead of dispatched. Nothing is lost — the work is still queued,
/// just not autonomously.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryConfig {
    /// Work Orders below this confidence file a story instead of
    /// dispatching. `"low"` disables routing (every approval dispatches, the
    /// pre-v0.5 behavior). Requires a configured tracker; without one the
    /// setting is inert and says so at startup.
    #[serde(default = "default_min_confidence_for_pr")]
    pub min_confidence_for_pr: GateConfidence,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        DeliveryConfig {
            min_confidence_for_pr: default_min_confidence_for_pr(),
        }
    }
}

fn default_min_confidence_for_pr() -> GateConfidence {
    // Medium, not High: the aim is to stop *gambles* becoming PRs, not to
    // route the ordinary case through a human. Eval run 4 showed the gate
    // rates conservatively — a High-only floor would send most real work to
    // the board and make the product feel broken.
    GateConfidence::Medium
}

/// Auto-dispatch settings. The trust posture of the whole product hangs on
/// the default here: **off** until an operator explicitly enables it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomyConfig {
    /// When true, Work Orders at or above `min_confidence` dispatch without
    /// a human click (the same safety verification still runs).
    #[serde(default)]
    pub auto_dispatch: bool,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: GateConfidence,
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        AutonomyConfig {
            auto_dispatch: false,
            min_confidence: GateConfidence::High,
        }
    }
}

/// Token-spend budget: gate calls plus runner-reported spend, per rolling
/// 24h window. Exceeding it halts gate evaluation (candidates stay Pending)
/// and pauses auto-dispatch until the window rolls.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// 0 = unlimited (the shipped default; caps are opt-in).
    #[serde(default)]
    pub max_tokens_per_day: u64,
}

fn default_min_confidence() -> GateConfidence {
    GateConfidence::High
}

impl GateConfig {
    pub fn diff_budget(&self) -> merge0_signal::DiffBudget {
        merge0_signal::DiffBudget {
            max_files: self.diff_max_files,
            max_total_lines: self.diff_max_total_lines,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_evidence_items() -> usize {
    8
}

fn default_max_section_chars() -> usize {
    2000
}

fn default_max_files() -> u32 {
    merge0_signal::DiffBudget::default().max_files
}

fn default_max_total_lines() -> u32 {
    merge0_signal::DiffBudget::default().max_total_lines
}

fn default_prior_attempts_cap() -> usize {
    5
}

fn default_stale_prior_days() -> u32 {
    90
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
}

pub fn load_scout(path: &Path) -> Result<ScoutConfig, ConfigError> {
    parse(path)
}

/// Load every `*.toml` scout in a directory, sorted by file name for
/// deterministic ordering.
pub fn load_scouts(dir: &Path) -> Result<Vec<ScoutConfig>, ConfigError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ConfigError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths.iter().map(|path| load_scout(path)).collect()
}

pub fn load_gate(path: &Path) -> Result<GateConfig, ConfigError> {
    parse(path)
}

fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source: Box::new(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_config() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config")
    }

    #[test]
    fn repo_scout_configs_are_valid() {
        let scouts = load_scouts(&repo_config().join("scouts")).expect("scout configs must parse");
        assert!(!scouts.is_empty(), "at least one scout must be configured");
        for scout in &scouts {
            assert!(
                !scout.prompt.trim().is_empty(),
                "{}: empty prompt",
                scout.name
            );
            assert!(!scout.sources.is_empty(), "{}: no sources", scout.name);
            // query_template is EXECUTED (audit M7) — every shipped
            // template must parse, or triage runs fail at runtime.
            crate::scouts::parse_query(scout).expect("shipped query_template must parse");
        }
        // The PRD's fourth standing question ships as config (PRD §4).
        let funnel = scouts
            .iter()
            .find(|s| s.name == "funnel-dropoff")
            .expect("funnel-dropoff scout must ship");
        assert_eq!(funnel.schedule, "weekly");
        // Every funnel-capable analytics source rides this scout. Contains,
        // not equals: pinning the exact list is how mixpanel shipped
        // unreachable — the reachability rule in merge0-e2e's repo_hygiene
        // owns completeness now.
        for source in [Source::Posthog, Source::Mixpanel] {
            assert!(
                funnel.sources.contains(&source),
                "funnel-dropoff must select {source:?}"
            );
        }
    }

    #[test]
    fn repo_gate_config_is_valid() {
        let gate = load_gate(&repo_config().join("gate.toml")).expect("gate config must parse");
        assert!(!gate.prompt.trim().is_empty());
        assert!(gate.max_work_orders_per_run > 0);
        // The trust posture: the SHIPPED config must never enable autonomy
        // or a spend cap surprise.
        assert!(!gate.autonomy.auto_dispatch, "auto-dispatch must ship off");
        assert_eq!(gate.autonomy.min_confidence, GateConfidence::High);
        assert_eq!(gate.budget.max_tokens_per_day, 0, "caps are opt-in");
    }

    #[test]
    fn autonomy_and_budget_default_off_when_absent() {
        let gate: GateConfig = toml::from_str(
            r#"
            prompt = "p"
            min_severity = "medium"
            max_work_orders_per_run = 3
            "#,
        )
        .unwrap();
        assert!(!gate.autonomy.auto_dispatch);
        assert_eq!(gate.autonomy.min_confidence, GateConfidence::High);
        assert_eq!(gate.budget.max_tokens_per_day, 0);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // deny_unknown_fields: a typo'd key must fail loudly, not silently
        // no-op — config drift breaks CI, not runtime.
        let result: Result<GateConfig, _> = toml::from_str(
            r#"
            prompt = "p"
            min_severity = "medium"
            max_work_orders_per_run = 3
            max_work_odrers = 5
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn enabled_defaults_to_true() {
        let scout: ScoutConfig = toml::from_str(
            r#"
            name = "x"
            description = "d"
            schedule = "nightly"
            sources = ["sentry"]
            query_template = "*"
            prompt = "p"
            "#,
        )
        .unwrap();
        assert!(scout.enabled);
    }
}
