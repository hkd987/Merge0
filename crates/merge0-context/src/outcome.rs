//! Outcome memory assembly: fingerprint history → `prior_attempts`.

use merge0_signal::OutcomeRef;
use merge0_store::{StoreError, TenantStore};

/// Collect prior attempts across a report's fingerprints, deduplicated by
/// (work order, outcome) and capped — a noisy history must not blow the Work
/// Order's context budget.
pub async fn prior_attempts(
    store: &TenantStore,
    fingerprints: &[String],
    cap: usize,
) -> Result<Vec<OutcomeRef>, StoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut attempts: Vec<OutcomeRef> = Vec::new();
    for fingerprint in fingerprints {
        for outcome in store.outcomes_for_fingerprint(fingerprint).await? {
            let key = (outcome.work_order_id, outcome.outcome);
            if seen.insert(key) {
                attempts.push(outcome);
            }
        }
    }
    attempts.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at));
    attempts.truncate(cap);
    Ok(attempts)
}
