//! Outcome reconciliation: webhooks are the fast path for PR fates, but a
//! missed delivery (restart, outage, expired hook) must not corrupt the
//! merge rate — the metric the whole product is judged by. Before each
//! triage run, every dispatch whose PR we still believe open is polled
//! against GitHub and any missed merged/closed outcome is recorded through
//! the same path the webhook would have used. Best-effort by design: a
//! GitHub hiccup here logs and moves on, and the next run sweeps again.

use super::webhooks::spawn_hardening_pass;
use crate::AppState;
use merge0_signal::{OutcomeKind, ReportStatus};

/// Poll open dispatches and repair missed outcomes. Returns a human-readable
/// action per repair (empty when everything was already consistent).
pub async fn reconcile_outcomes(state: &AppState) -> Vec<String> {
    let mut actions = Vec::new();
    let open = match state.tenant.open_pr_dispatches().await {
        Ok(open) => open,
        Err(e) => {
            tracing::warn!("reconcile: listing open dispatches failed: {e}");
            return actions;
        }
    };
    for (report_id, pr_url) in open {
        let Some(number) = pr_number(&pr_url) else {
            tracing::warn!(report = %report_id, pr_url, "reconcile: unparseable PR url");
            continue;
        };
        let pull = match state.github.get_pull_request(&state.repo, number).await {
            Ok(pull) => pull,
            Err(e) => {
                tracing::warn!(report = %report_id, "reconcile: PR #{number} fetch failed: {e}");
                continue;
            }
        };
        if pull.state != "closed" {
            continue; // genuinely still open — nothing to repair
        }
        let result = if pull.merged {
            let when = pull.merged_at.unwrap_or_else(chrono::Utc::now);
            if let Some(sha) = &pull.merge_commit_sha {
                if let Err(e) = state.tenant.record_merge_sha(report_id, sha).await {
                    tracing::warn!(report = %report_id, "reconcile: merge sha record failed: {e}");
                }
            }
            let tokens = state
                .tenant
                .dispatch(report_id)
                .await
                .ok()
                .flatten()
                .and_then(|d| d.tokens_spent);
            state
                .tenant
                .record_outcome(
                    report_id,
                    OutcomeKind::Merged,
                    Some(&pr_url),
                    when,
                    None,
                    tokens,
                )
                .await
        } else {
            let when = pull.closed_at.unwrap_or_else(chrono::Utc::now);
            state
                .tenant
                .record_outcome(
                    report_id,
                    OutcomeKind::Closed,
                    Some(&pr_url),
                    when,
                    None,
                    None,
                )
                .await
        };
        match result {
            Ok(true) => {
                if let Err(e) = state
                    .tenant
                    .set_report_status(report_id, ReportStatus::Completed)
                    .await
                {
                    tracing::warn!(report = %report_id, "reconcile: status update failed: {e}");
                }
                let kind = if pull.merged { "merged" } else { "closed" };
                tracing::info!(report = %report_id, "reconciled missed {kind} outcome for PR #{number}");
                actions.push(format!(
                    "reconciled missed {kind} outcome for report {report_id}"
                ));
                if pull.merged && state.hardening_enabled {
                    spawn_hardening_pass(state.clone());
                }
            }
            Ok(false) => {} // outcome already recorded (webhook won the race)
            Err(e) => {
                tracing::warn!(report = %report_id, "reconcile: outcome record failed: {e}");
            }
        }
    }
    actions
}

/// `https://github.com/owner/repo/pull/128` → `128`.
fn pr_number(pr_url: &str) -> Option<u64> {
    pr_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::pr_number;

    #[test]
    fn pr_numbers_parse_from_urls_and_garbage_is_none() {
        assert_eq!(pr_number("https://github.com/a/b/pull/128"), Some(128));
        assert_eq!(pr_number("https://github.com/a/b/pull/7/"), Some(7));
        assert_eq!(pr_number("https://github.com/a/b/pulls"), None);
        assert_eq!(pr_number(""), None);
    }
}
