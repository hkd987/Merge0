//! Metering & billing (PRD split table: "Billing, usage metering, hosted
//! convenience" is `/ee` + hosted).
//!
//! The PRD's Future Considerations leave the pricing model open —
//! "per-merged-PR pricing below $15, or flat monthly with PR pool (final
//! model is an open question)" — so BOTH models are implemented as pure
//! functions, letting the Phase 2 pricing experiment A/B them over the same
//! [`Usage`] numbers. Invoicing is pure; only [`usage`] touches the store
//! (through the tenant's own telemetry query, so metering and the customer's
//! dashboard can never disagree).

use crate::{EeError, Result, Tenant, TenantManager};
use chrono::{DateTime, Utc};

/// The market price a per-PR rate must undercut (PRD Problem Statement:
/// "$1–5 in tokens against a $15/PR market price").
pub const MARKET_PRICE_CENTS: u32 = 1500;

/// The two Phase 2 pricing experiments. Construct via the `try_new_*`
/// validators — both per-PR rates must undercut [`MARKET_PRICE_CENTS`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pricing {
    /// "You only pay for real work": merged PRs × a per-PR rate.
    PerMergedPr { cents_per_pr: u32 },
    /// Flat monthly fee including a PR pool, with per-PR overage.
    FlatPlusPool {
        monthly_cents: u32,
        included_prs: u32,
        overage_cents_per_pr: u32,
    },
}

impl Pricing {
    pub fn try_new_per_merged_pr(cents_per_pr: u32) -> Result<Pricing> {
        check_undercuts_market("per-merged-PR rate", cents_per_pr)?;
        Ok(Pricing::PerMergedPr { cents_per_pr })
    }

    pub fn try_new_flat_plus_pool(
        monthly_cents: u32,
        included_prs: u32,
        overage_cents_per_pr: u32,
    ) -> Result<Pricing> {
        check_undercuts_market("overage rate", overage_cents_per_pr)?;
        Ok(Pricing::FlatPlusPool {
            monthly_cents,
            included_prs,
            overage_cents_per_pr,
        })
    }
}

fn check_undercuts_market(what: &str, cents: u32) -> Result<()> {
    if cents >= MARKET_PRICE_CENTS {
        return Err(EeError::InvalidPricing(format!(
            "{what} must be below the ${}/PR market price, got {cents}¢",
            MARKET_PRICE_CENTS / 100
        )));
    }
    Ok(())
}

/// One billing period, itemized. `total_cents` is exact (u64); each line
/// amount is clamped into u32 (unreachable at sane volumes — it would take
/// >4M merged PRs on one invoice).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Invoice {
    pub line_items: Vec<(String, u32)>,
    pub total_cents: u64,
}

/// Price a period's merged-PR count under either model. Pure — the caller
/// supplies the count (see [`usage`]).
pub fn invoice(pricing: &Pricing, merged_prs: u64) -> Invoice {
    match *pricing {
        Pricing::PerMergedPr { cents_per_pr } => {
            let amount = merged_prs.saturating_mul(u64::from(cents_per_pr));
            Invoice {
                line_items: vec![(
                    format!("{merged_prs} merged PRs × {cents_per_pr}¢"),
                    clamp_cents(amount),
                )],
                total_cents: amount,
            }
        }
        Pricing::FlatPlusPool {
            monthly_cents,
            included_prs,
            overage_cents_per_pr,
        } => {
            let mut line_items = vec![(
                format!("monthly fee ({included_prs} merged PRs included)"),
                monthly_cents,
            )];
            let overage = merged_prs.saturating_sub(u64::from(included_prs));
            let overage_amount = overage.saturating_mul(u64::from(overage_cents_per_pr));
            if overage > 0 {
                line_items.push((
                    format!("{overage} PRs over pool × {overage_cents_per_pr}¢"),
                    clamp_cents(overage_amount),
                ));
            }
            Invoice {
                line_items,
                total_cents: u64::from(monthly_cents).saturating_add(overage_amount),
            }
        }
    }
}

fn clamp_cents(cents: u64) -> u32 {
    u32::try_from(cents).unwrap_or(u32::MAX)
}

/// A tenant's billable usage over a rolling window, read from its own
/// telemetry (`TenantStore::telemetry`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub window_days: u32,
    /// The billable unit under both pricing models.
    pub merged_prs: u64,
    /// Work Orders dispatched to the tenant's compute in the window.
    pub dispatched: u64,
    /// Test-passing PRs opened in the window.
    pub prs_opened: u64,
    /// Tokens spent on runs whose PR merged (PRD P2 cost accounting).
    pub tokens_spent: u64,
}

/// Meter one tenant. Fails with [`EeError::TenantSuspended`] for suspended
/// tenants (metering goes through the same guarded door as everything else).
pub async fn usage(
    manager: &TenantManager,
    tenant: &Tenant,
    window_days: u32,
    now: DateTime<Utc>,
) -> Result<Usage> {
    let store = manager.tenant_store(tenant).await?;
    let snapshot = store.telemetry(window_days, 3, now).await?;
    Ok(Usage {
        window_days,
        merged_prs: snapshot.counts.prs_merged,
        dispatched: snapshot.counts.dispatched,
        prs_opened: snapshot.counts.prs_opened,
        tokens_spent: snapshot.counts.tokens_on_merged.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_merged_pr_charges_only_for_real_work() {
        let pricing = Pricing::try_new_per_merged_pr(500).unwrap();
        let inv = invoice(&pricing, 7);
        assert_eq!(inv.total_cents, 3500);
        assert_eq!(
            inv.line_items,
            vec![("7 merged PRs × 500¢".to_string(), 3500)]
        );
    }

    #[test]
    fn zero_merged_prs_costs_nothing_per_pr_but_full_flat_fee() {
        let per_pr = Pricing::try_new_per_merged_pr(500).unwrap();
        assert_eq!(invoice(&per_pr, 0).total_cents, 0);

        let flat = Pricing::try_new_flat_plus_pool(9900, 20, 400).unwrap();
        let inv = invoice(&flat, 0);
        assert_eq!(inv.total_cents, 9900);
        assert_eq!(inv.line_items.len(), 1, "no overage line at zero merged");
    }

    #[test]
    fn flat_plus_pool_boundary_exactly_at_pool_size() {
        let flat = Pricing::try_new_flat_plus_pool(9900, 20, 400).unwrap();
        // Exactly at the pool: no overage.
        let at = invoice(&flat, 20);
        assert_eq!(at.total_cents, 9900);
        assert_eq!(at.line_items.len(), 1);
        // One past the pool: one overage PR.
        let over = invoice(&flat, 21);
        assert_eq!(over.total_cents, 9900 + 400);
        assert_eq!(over.line_items.len(), 2);
        assert_eq!(
            over.line_items[1],
            ("1 PRs over pool × 400¢".to_string(), 400)
        );
    }

    #[test]
    fn flat_plus_pool_overage_scales() {
        let flat = Pricing::try_new_flat_plus_pool(9900, 20, 400).unwrap();
        let inv = invoice(&flat, 25);
        assert_eq!(inv.total_cents, 9900 + 5 * 400);
    }

    #[test]
    fn validator_rejects_rates_at_or_above_market_price() {
        assert!(Pricing::try_new_per_merged_pr(1499).is_ok());
        for bad in [1500, 1501, u32::MAX] {
            assert!(matches!(
                Pricing::try_new_per_merged_pr(bad),
                Err(EeError::InvalidPricing(_))
            ));
            assert!(matches!(
                Pricing::try_new_flat_plus_pool(50_000, 100, bad),
                Err(EeError::InvalidPricing(_))
            ));
        }
        // The flat fee itself is not per-PR and may exceed $15.
        assert!(Pricing::try_new_flat_plus_pool(99_900, 100, 1499).is_ok());
    }

    #[test]
    fn totals_do_not_overflow_on_absurd_volume() {
        let pricing = Pricing::try_new_per_merged_pr(1499).unwrap();
        let inv = invoice(&pricing, u64::MAX);
        assert_eq!(inv.total_cents, u64::MAX, "saturates instead of panicking");
        assert_eq!(inv.line_items[0].1, u32::MAX);
    }
}
