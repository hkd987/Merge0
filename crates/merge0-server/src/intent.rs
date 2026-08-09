//! Intent-doc resolution (PRD §3, audit finding C5): the customer's
//! MERGE0.md is fetched from THEIR repo per triage run — editing the doc in
//! the repo takes effect on the next run, no server restart. The configured
//! fallback covers repos without a MERGE0.md yet.
//!
//! The **whole** doc travels to the gate, machine fence included. This used
//! to hand over `human_text()`, which strips the fence — and the fence is
//! precisely where `merge0-hardening` writes its `IntentAmendment`s. That
//! made mechanism 3 of the hardening hierarchy write to a location nothing
//! read: constraints earned from real incidents never reached a decision.
//! Selecting and labelling the fence is
//! [`merge0_context::intent::relevant_intent`]'s job, not this function's.

use crate::AppState;

pub const INTENT_DOC_PATH: &str = "MERGE0.md";

/// Fetch the live intent doc; fall back (with a warning) on fetch errors or
/// absence so triage never stalls on a GitHub hiccup.
pub async fn resolve_intent(state: &AppState) -> String {
    match state
        .github
        .get_file_content(&state.repo, INTENT_DOC_PATH)
        .await
    {
        Ok(Some(text)) => text,
        Ok(None) => state.intent_fallback.as_ref().clone(),
        Err(e) => {
            tracing::warn!("MERGE0.md fetch failed, using fallback intent: {e}");
            state.intent_fallback.as_ref().clone()
        }
    }
}
