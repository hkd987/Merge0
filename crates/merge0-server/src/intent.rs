//! Intent-doc resolution (PRD §3, audit finding C5): the customer's
//! MERGE0.md is fetched from THEIR repo per triage run — editing the doc in
//! the repo takes effect on the next run, no server restart. Fence-stripped
//! human prose feeds the gate; the configured fallback covers repos without
//! a MERGE0.md yet.

use crate::AppState;
use merge0_context::intent::IntentDoc;

pub const INTENT_DOC_PATH: &str = "MERGE0.md";

/// Fetch + parse the live intent doc; fall back (with a warning) on fetch
/// errors or absence so triage never stalls on a GitHub hiccup.
pub async fn resolve_intent(state: &AppState) -> String {
    match state
        .github
        .get_file_content(&state.repo, INTENT_DOC_PATH)
        .await
    {
        Ok(Some(text)) => match IntentDoc::parse(&text) {
            Ok(doc) => doc.human_text(),
            // A doc without the machine fence is customer prose throughout.
            Err(_) => text,
        },
        Ok(None) => state.intent_fallback.as_ref().clone(),
        Err(e) => {
            tracing::warn!("MERGE0.md fetch failed, using fallback intent: {e}");
            state.intent_fallback.as_ref().clone()
        }
    }
}
