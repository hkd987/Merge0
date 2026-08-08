//! Shared approve/dismiss actions — one implementation behind the HTTP
//! inbox, the Slack interaction endpoint, and any future surface.

use super::ApiError;
use crate::AppState;
use chrono::Utc;
use merge0_github::safety::verify_repo_safety;
use merge0_runner::{ActionsRunner, AgentManifest, Runner};
use merge0_signal::{DismissReason, ReportStatus};
use ulid::Ulid;

/// Who pulled the dispatch trigger — recorded on every dispatch (the
/// autonomy dial's audit trail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchedBy {
    /// The web inbox.
    Human,
    /// A Slack interaction.
    Slack,
    /// The autonomy dial (auto-dispatch above the confidence threshold).
    Auto,
}

impl DispatchedBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DispatchedBy::Human => "human",
            DispatchedBy::Slack => "slack",
            DispatchedBy::Auto => "auto",
        }
    }
}

/// Approve → verify safety (P0-9) → transactional verdict+dispatch record →
/// `repository_dispatch` (P0-6), with rollback to the inbox if the dispatch
/// API call fails.
pub async fn approve(
    state: &AppState,
    id: Ulid,
    by: DispatchedBy,
) -> Result<serde_json::Value, ApiError> {
    let report = state.tenant.get_report(id).await?;
    if report.status != ReportStatus::AwaitingReview {
        return Err(ApiError::conflict(format!(
            "report is {:?}, only awaiting_review reports can be approved",
            report.status
        )));
    }
    let Some(work_order) = state.tenant.work_order(id).await? else {
        return Err(ApiError::conflict("report has no work order"));
    };

    // P0-9: refuse to dispatch until branch protection + required CI are
    // confirmed — verified fresh at every approval.
    let safety = verify_repo_safety(state.github.as_ref(), &state.repo)
        .await
        .map_err(ApiError::internal)?;
    if !safety.satisfied() {
        return Err(ApiError::conflict(format!(
            "safety verification failed: {}",
            safety.failures().join("; ")
        )));
    }

    // Manifest attribution (PRD §5b): fetched from the customer repo at
    // dispatch time; a repo without a manifest dispatches with defaults.
    let manifest = fetch_manifest(state).await?;
    let attribution = manifest.attribution();

    let now = Utc::now();
    state
        .tenant
        .approve_for_dispatch(
            id,
            state.agent.label(),
            Some(&attribution),
            by.as_str(),
            now,
        )
        .await?;

    let runner = ActionsRunner {
        api: state.github.clone(),
        agent: state.agent.clone(),
        callback_url: state.callback_url.clone(),
        attribution: Some(attribution),
    };
    match runner.dispatch(&work_order).await {
        Ok(receipt) => {
            // Register the broker grant (PRD §5a): exactly one credential
            // for exactly this repo, drawable by this Work Order.
            if let Some(broker) = &state.broker {
                broker.lock().await.register_grant_for(&work_order);
            }
            Ok(serde_json::json!({
                "approved": id.to_string(),
                "dispatched_to": receipt.repo.full(),
                "runner": receipt.runner_kind,
                "dispatched_by": by.as_str(),
            }))
        }
        Err(e) => {
            // Return the report to the inbox so the approval can be retried.
            state.tenant.rollback_dispatch(id).await?;
            Err(ApiError::internal(format!(
                "dispatch failed (approval rolled back): {e}"
            )))
        }
    }
}

pub async fn dismiss(
    state: &AppState,
    id: Ulid,
    reason: DismissReason,
) -> Result<serde_json::Value, ApiError> {
    state.tenant.dismiss_report(id, reason, Utc::now()).await?;
    Ok(serde_json::json!({
        "dismissed": id.to_string(),
        "reason": reason,
    }))
}

/// Load `.merge0/agent.toml` from the customer repo; absent → defaults; a
/// malformed manifest is a hard error (silently ignoring customer config
/// would be worse than failing the approval).
pub async fn fetch_manifest(state: &AppState) -> Result<AgentManifest, ApiError> {
    match state
        .github
        .get_file_content(&state.repo, merge0_runner::manifest::MANIFEST_PATH)
        .await
    {
        Ok(Some(text)) => AgentManifest::parse(&text)
            .map_err(|e| ApiError::conflict(format!(".merge0/agent.toml is invalid: {e}"))),
        Ok(None) => Ok(AgentManifest::default()),
        Err(e) => Err(ApiError::internal(format!("manifest fetch failed: {e}"))),
    }
}
