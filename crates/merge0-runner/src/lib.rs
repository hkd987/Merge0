//! Runner interface (PRD §5): BYO agent, BYO compute.
//!
//! The interface is agent-agnostic by design — `WorkOrder in → PRResult out`
//! — so alternative agents are a config change, not a rewrite. The v1
//! implementation (GitHub Actions `repository_dispatch` + headless Claude
//! Code) lands in a follow-up branch; this crate currently pins down the
//! contract.

use merge0_signal::WorkOrder;
use serde::{Deserialize, Serialize};

/// Terminal state of a dispatched run. A failed or discarded run opens no PR
/// (no red PRs reach the inbox) but still writes an outcome plus its salvage
/// diagnosis to outcome memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PrResult {
    /// Test-passing PR opened for human review.
    Opened { pr_url: String, branch: String },
    /// Run discarded itself: repair budget exhausted or diff budget exceeded.
    /// `diagnosis` is the failed-run salvage attached to the Report.
    Discarded { reason: String, diagnosis: String },
    /// Infrastructure failure (workflow never ran, dispatch rejected, ...).
    Failed { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("dispatch failed: {0}")]
    Dispatch(String),
}

/// Dispatch an approved Work Order to customer-side compute.
pub trait Runner {
    fn dispatch(&self, order: &WorkOrder) -> Result<PrResult, RunnerError>;
}
