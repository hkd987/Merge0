//! Cross-tenant outcome priors — the hosted-only *data service* (PRD split
//! table: "anonymized, aggregated 'what kinds of fixes merge' intelligence
//! that improves gate precision"; Future Considerations: built "once ≥2
//! tenants exist").
//!
//! ANONYMIZATION INVARIANT: [`GatePriors`] carries *only* bucket keys
//! (severity × source-mix) and counts. No tenant ids, schema names, repo
//! names, report titles, fingerprints, or URLs ever enter the structure —
//! the integration tests serialize it and assert exactly that. This is what
//! makes the aggregate shareable back to every tenant's gate.

use crate::{enum_str, Result, TenantManager};
use chrono::{DateTime, Duration, Utc};
use merge0_signal::{OutcomeKind, Severity};
use std::collections::{BTreeMap, HashSet};

/// Minimum attempts before a bucket's merge rate is considered signal
/// rather than noise ([`GatePriors::advice`] returns `None` below this).
pub const MIN_PRIOR_ATTEMPTS: u64 = 5;

/// One anonymized bucket: how often fixes of this shape reached a terminal
/// PR outcome, and how often that outcome was a merge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PriorBucket {
    pub attempts: u64,
    pub merged: u64,
}

/// Aggregated priors over every non-suspended tenant, keyed by
/// `<severity>/<single_source|cross_source>` (e.g. `high/cross_source`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GatePriors {
    pub buckets: BTreeMap<String, PriorBucket>,
}

/// The bucket key for a report shape: its severity and whether its member
/// signals corroborate across more than one source.
pub fn bucket_key(severity: Severity, cross_source: bool) -> String {
    let mix = if cross_source {
        "cross_source"
    } else {
        "single_source"
    };
    format!("{}/{mix}", enum_str(&severity))
}

impl GatePriors {
    /// Merge rate for a bucket, or `None` when the bucket is missing or has
    /// fewer than [`MIN_PRIOR_ATTEMPTS`] attempts (too small a sample to
    /// steer a gate).
    pub fn advice(&self, severity: Severity, cross_source: bool) -> Option<f64> {
        let bucket = self.buckets.get(&bucket_key(severity, cross_source))?;
        (bucket.attempts >= MIN_PRIOR_ATTEMPTS)
            .then(|| bucket.merged as f64 / bucket.attempts as f64)
    }

    /// A compact text block a hosted gate can append to its prompt. Only
    /// buckets meeting the minimum sample appear — the gate should never see
    /// noise dressed up as a prior.
    pub fn as_gate_context(&self) -> String {
        let lines: Vec<String> = self
            .buckets
            .iter()
            .filter(|(_, b)| b.attempts >= MIN_PRIOR_ATTEMPTS)
            .map(|(key, b)| {
                let rate = 100.0 * b.merged as f64 / b.attempts as f64;
                format!("{key} merges at {rate:.0}% (n={})", b.attempts)
            })
            .collect();
        if lines.is_empty() {
            "historical priors: insufficient data".to_string()
        } else {
            format!("historical priors: {}", lines.join("; "))
        }
    }
}

/// Aggregate priors across every non-suspended tenant: each report's
/// terminal PR outcomes (merged / closed / reverted — discarded runs never
/// opened a PR) inside the window are bucketed by the report's severity and
/// whether its member signals span more than one source.
pub async fn compute_priors(
    manager: &TenantManager,
    window_days: u32,
    now: DateTime<Utc>,
) -> Result<GatePriors> {
    let cutoff = now - Duration::days(i64::from(window_days));
    let mut priors = GatePriors::default();

    for tenant in manager.list_tenants().await? {
        if tenant.suspended {
            continue;
        }
        let store = manager.tenant_store(&tenant).await?;
        for report in store.list_reports(None).await? {
            let outcomes = store.outcomes_for_report(report.id).await?;
            let terminal: Vec<_> = outcomes
                .iter()
                .filter(|o| {
                    o.occurred_at >= cutoff
                        && matches!(
                            o.outcome,
                            OutcomeKind::Merged | OutcomeKind::Closed | OutcomeKind::Reverted
                        )
                })
                .collect();
            if terminal.is_empty() {
                continue;
            }
            let signals = store.report_signals(report.id).await?;
            let sources: HashSet<_> = signals.iter().map(|s| s.source).collect();
            let key = bucket_key(report.severity, sources.len() > 1);
            let bucket = priors.buckets.entry(key).or_default();
            for outcome in terminal {
                bucket.attempts += 1;
                if outcome.outcome == OutcomeKind::Merged {
                    bucket.merged += 1;
                }
            }
        }
    }
    Ok(priors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priors_with(key: &str, attempts: u64, merged: u64) -> GatePriors {
        let mut priors = GatePriors::default();
        priors
            .buckets
            .insert(key.to_string(), PriorBucket { attempts, merged });
        priors
    }

    #[test]
    fn bucket_keys_join_severity_and_source_mix() {
        assert_eq!(bucket_key(Severity::High, true), "high/cross_source");
        assert_eq!(bucket_key(Severity::Low, false), "low/single_source");
        assert_eq!(
            bucket_key(Severity::Critical, true),
            "critical/cross_source"
        );
    }

    #[test]
    fn advice_requires_minimum_sample() {
        // One under the threshold: no advice.
        let thin = priors_with("high/cross_source", MIN_PRIOR_ATTEMPTS - 1, 4);
        assert_eq!(thin.advice(Severity::High, true), None);
        // Exactly at the threshold: advice appears.
        let enough = priors_with("high/cross_source", MIN_PRIOR_ATTEMPTS, 4);
        assert_eq!(enough.advice(Severity::High, true), Some(0.8));
        // Missing bucket: no advice.
        assert_eq!(enough.advice(Severity::Low, false), None);
    }

    #[test]
    fn gate_context_reports_rates_and_hides_thin_buckets() {
        let mut priors = priors_with("high/cross_source", 32, 25);
        priors.buckets.insert(
            "low/single_source".to_string(),
            PriorBucket {
                attempts: 2,
                merged: 2,
            },
        );
        let context = priors.as_gate_context();
        assert_eq!(
            context,
            "historical priors: high/cross_source merges at 78% (n=32)"
        );
        assert!(!context.contains("low/single_source"), "thin bucket hidden");
    }

    #[test]
    fn gate_context_states_when_there_is_no_data() {
        assert_eq!(
            GatePriors::default().as_gate_context(),
            "historical priors: insufficient data"
        );
    }
}
