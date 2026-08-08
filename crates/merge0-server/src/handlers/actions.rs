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

/// What approval delivers. A team that has not yet earned trust in
/// autonomous code can take the same evidence-backed Work Order as a
/// tracker story instead of a PR, and turn PRs on later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    /// Dispatch the runner; the PR is the artifact. The shipped default.
    #[default]
    Pr,
    /// File a tracker story and stop. No runner, no PR — the report is
    /// terminally handed off to whoever owns the board.
    Story,
    /// File a tracker story *and* dispatch, so the board reflects work the
    /// agent is already doing.
    StoryAndPr,
}

impl DeliveryMode {
    /// Parse `MERGE0_DELIVERY_MODE`. Unknown values fall back to `Pr` and
    /// are reported by the caller — a typo must not silently change what
    /// approval does.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pr" => Some(DeliveryMode::Pr),
            "story" => Some(DeliveryMode::Story),
            "story_and_pr" => Some(DeliveryMode::StoryAndPr),
            _ => None,
        }
    }

    fn files_story(&self) -> bool {
        matches!(self, DeliveryMode::Story | DeliveryMode::StoryAndPr)
    }

    fn dispatches(&self) -> bool {
        matches!(self, DeliveryMode::Pr | DeliveryMode::StoryAndPr)
    }
}

/// Approve → verify safety (P0-9) → transactional verdict+dispatch record →
/// `repository_dispatch` (P0-6), with rollback to the inbox if the dispatch
/// API call fails.
///
/// Under a story-filing delivery mode the story is created first, because
/// the failure semantics differ by mode and both are deliberate:
///
/// - **story** — the story is the ONLY artifact, so a tracker failure fails
///   the approval. Reporting success while having delivered nothing would
///   be a lie, and the report stays in the inbox to retry.
/// - **story_and_pr** — the PR is the artifact and the story is companion
///   metadata, so a tracker outage logs and the dispatch proceeds. Blocking
///   a fix on the board being up would be the wrong trade.
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

    // Story delivery, when configured. Filed before anything else changes
    // state so a failure in story-only mode leaves the report untouched in
    // the inbox rather than half-approved.
    let mut story: Option<merge0_tracker::CreatedStory> = None;
    if state.delivery_mode.files_story() {
        // Idempotency: an earlier attempt that filed a story then failed
        // must not file a second one on retry.
        if let Some((key, url)) = state.tenant.report_story(id).await? {
            story = Some(merge0_tracker::CreatedStory { key, url });
        } else {
            match file_story(state, &work_order).await {
                Ok(created) => {
                    state
                        .tenant
                        .set_report_story(id, &created.key, &created.url)
                        .await?;
                    story = Some(created);
                }
                Err(e) if state.delivery_mode == DeliveryMode::Story => {
                    // The story WAS the delivery — fail loudly, keep the
                    // report reviewable.
                    return Err(ApiError::internal(format!(
                        "story delivery failed, report left in the inbox: {e}"
                    )));
                }
                Err(e) => {
                    tracing::warn!(
                        report = %id,
                        "tracker story failed; dispatching the PR anyway: {e}"
                    );
                }
            }
        }
    }

    // Story-only delivery is terminal: no runner, no PR, so none of the
    // dispatch machinery below applies.
    if !state.delivery_mode.dispatches() {
        let created = story.expect("story mode returns early on failure");
        let brief = format!(
            "Filed as tracker story {} ({}).\n\n{}",
            created.key, created.url, work_order.summary
        );
        state.tenant.hand_off_report(id, &brief, Utc::now()).await?;
        return Ok(serde_json::json!({
            "approved": id.to_string(),
            "delivered_as": "story",
            "story_key": created.key,
            "story_url": created.url,
            "approved_by": by.as_str(),
        }));
    }

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
                "delivered_as": if story.is_some() { "story_and_pr" } else { "pr" },
                "story_key": story.as_ref().map(|s| s.key.clone()),
                "story_url": story.as_ref().map(|s| s.url.clone()),
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

/// Render and file the story for an approved Work Order.
async fn file_story(
    state: &AppState,
    work_order: &merge0_signal::WorkOrder,
) -> Result<merge0_tracker::CreatedStory, String> {
    let tracker = state
        .tracker
        .as_ref()
        .ok_or("no tracker configured — set MERGE0_DELIVERY_MODE=pr or configure Jira")?;
    let story = merge0_tracker::story_from_work_order(work_order, state.gate.max_evidence_items);
    tracker
        .create_story(&story)
        .await
        .map_err(|e| e.to_string())
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
