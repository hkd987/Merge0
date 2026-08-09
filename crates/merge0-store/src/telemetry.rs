//! Acceptance-rate telemetry queries (PRD P0-10). SQL produces raw counts;
//! the rate math lives in `merge0_signal::telemetry` where it is unit-tested
//! without a database.

use crate::{enum_str, Result, TenantStore};
use chrono::{DateTime, Duration, Utc};
use merge0_signal::telemetry::{TelemetryCounts, TelemetrySnapshot};
use merge0_signal::{OutcomeKind, ReportKind, ReportStatus};
use sqlx::Row;
use std::collections::BTreeMap;

impl TenantStore {
    pub async fn telemetry(
        &self,
        window_days: u32,
        efficacy_grace_days: u32,
        now: DateTime<Utc>,
    ) -> Result<TelemetrySnapshot> {
        let cutoff = now - Duration::days(window_days as i64);

        let dispatches = self.table("dispatches");
        let outcomes = self.table("outcomes");
        let reports = self.table("reports");
        let report_signals = self.table("report_signals");
        let signals = self.table("signals");

        let dispatched: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {dispatches} WHERE dispatched_at >= $1"
        ))
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;

        let prs_opened: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {dispatches} WHERE pr_opened_at >= $1"
        ))
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;

        let outcome_count = |kind: OutcomeKind| {
            let sql =
                format!("SELECT COUNT(*) FROM {outcomes} WHERE kind = $1 AND occurred_at >= $2");
            let kind = enum_str(&kind);
            let pool = self.pool().clone();
            async move {
                let n: i64 = sqlx::query_scalar(&sql)
                    .bind(kind)
                    .bind(cutoff)
                    .fetch_one(&pool)
                    .await?;
                Ok::<i64, crate::StoreError>(n)
            }
        };
        let prs_merged = outcome_count(OutcomeKind::Merged).await?;
        let prs_closed = outcome_count(OutcomeKind::Closed).await?;
        let prs_reverted = outcome_count(OutcomeKind::Reverted).await?;
        let runs_discarded = outcome_count(OutcomeKind::Discarded).await?;

        // Approved = a human verdict that wasn't a dismissal, on maintenance
        // reports (opportunity handoffs are neither approve nor dismiss).
        let reports_approved: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {reports}
             WHERE decided_at >= $1 AND kind = $2
               AND status <> $3 AND dismiss_reason IS NULL"
        ))
        .bind(cutoff)
        .bind(enum_str(&ReportKind::Maintenance))
        .bind(enum_str(&ReportStatus::Dismissed))
        .fetch_one(self.pool())
        .await?;

        let dismissal_rows = sqlx::query(&format!(
            "SELECT dismiss_reason, COUNT(*) AS n FROM {reports}
             WHERE decided_at >= $1 AND dismiss_reason IS NOT NULL
             GROUP BY dismiss_reason"
        ))
        .bind(cutoff)
        .fetch_all(self.pool())
        .await?;
        let mut dismissals = BTreeMap::new();
        for row in &dismissal_rows {
            dismissals.insert(
                row.get::<String, _>("dismiss_reason"),
                row.get::<i64, _>("n") as u64,
            );
        }

        // Median seconds from PR open to terminal PR outcome.
        let median_time_to_review_secs: Option<f64> = sqlx::query_scalar(&format!(
            "SELECT PERCENTILE_CONT(0.5) WITHIN GROUP (
                 ORDER BY EXTRACT(EPOCH FROM o.occurred_at - d.pr_opened_at)
             )
             FROM {outcomes} o
             JOIN {dispatches} d ON d.report_id = o.report_id
             WHERE o.kind IN ($1,$2,$3) AND o.occurred_at >= $4 AND d.pr_opened_at IS NOT NULL"
        ))
        .bind(enum_str(&OutcomeKind::Merged))
        .bind(enum_str(&OutcomeKind::Closed))
        .bind(enum_str(&OutcomeKind::Reverted))
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;

        let tokens_on_merged: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT SUM(d.tokens_spent)::BIGINT
             FROM {outcomes} o
             JOIN {dispatches} d ON d.report_id = o.report_id
             WHERE o.kind = $1 AND o.occurred_at >= $2"
        ))
        .bind(enum_str(&OutcomeKind::Merged))
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;

        // Close-the-loop (fix efficacy): for each merged fix in the window,
        // did any member signal recur after the grace period? Confirmed only
        // once the grace period has fully elapsed.
        let efficacy_row = sqlx::query(&format!(
            "SELECT
                 COUNT(*) FILTER (WHERE NOT recurred AND grace_end < $2) AS confirmed,
                 COUNT(*) FILTER (WHERE recurred) AS recurred,
                 COUNT(*) FILTER (WHERE NOT recurred AND grace_end >= $2) AS pending
             FROM (
                 SELECT o.report_id,
                        o.occurred_at + make_interval(days => $3) AS grace_end,
                        EXISTS (
                            SELECT 1 FROM {report_signals} rs
                            JOIN {signals} s ON s.id = rs.signal_id
                            WHERE rs.report_id = o.report_id
                              AND s.last_seen > o.occurred_at + make_interval(days => $3)
                        ) AS recurred
                 FROM {outcomes} o
                 WHERE o.kind = $1 AND o.occurred_at >= $4
             ) fixes"
        ))
        .bind(enum_str(&OutcomeKind::Merged))
        .bind(now)
        .bind(efficacy_grace_days as i32)
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;
        let fixes_confirmed: i64 = efficacy_row.get("confirmed");
        let fixes_recurred: i64 = efficacy_row.get("recurred");
        let fixes_pending: i64 = efficacy_row.get("pending");

        let auto_dispatched: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {dispatches}
             WHERE dispatched_by = 'auto' AND dispatched_at >= $1"
        ))
        .bind(cutoff)
        .fetch_one(self.pool())
        .await?;

        let tokens_spent_24h = self.tokens_spent_since(now - Duration::hours(24)).await?;

        let counts = TelemetryCounts {
            window_days,
            dispatched: dispatched as u64,
            prs_opened: prs_opened as u64,
            prs_merged: prs_merged as u64,
            prs_closed: prs_closed as u64,
            prs_reverted: prs_reverted as u64,
            runs_discarded: runs_discarded as u64,
            reports_approved: reports_approved as u64,
            dismissals,
            median_time_to_review_secs: median_time_to_review_secs.map(|s| s as i64),
            tokens_on_merged: tokens_on_merged.map(|n| n as u64),
            fixes_confirmed: fixes_confirmed.max(0) as u64,
            fixes_recurred: fixes_recurred.max(0) as u64,
            fixes_pending: fixes_pending.max(0) as u64,
            auto_dispatched: auto_dispatched.max(0) as u64,
            tokens_spent_24h,
        };
        Ok(TelemetrySnapshot::from_counts(counts))
    }

    /// Post-merge efficacy verdict for ONE report (the detail endpoint's
    /// per-fix status). None when the report has no merged outcome.
    pub async fn fix_efficacy(
        &self,
        report_id: ulid::Ulid,
        efficacy_grace_days: u32,
        now: DateTime<Utc>,
    ) -> Result<Option<merge0_signal::telemetry::FixEfficacy>> {
        use merge0_signal::telemetry::FixEfficacy;
        let row = sqlx::query(&format!(
            "SELECT o.occurred_at + make_interval(days => $3) AS grace_end,
                    EXISTS (
                        SELECT 1 FROM {rs} rs
                        JOIN {signals} s ON s.id = rs.signal_id
                        WHERE rs.report_id = o.report_id
                          AND s.last_seen > o.occurred_at + make_interval(days => $3)
                    ) AS recurred
             FROM {outcomes} o
             WHERE o.report_id = $1 AND o.kind = $2",
            outcomes = self.table("outcomes"),
            rs = self.table("report_signals"),
            signals = self.table("signals"),
        ))
        .bind(report_id.to_string())
        .bind(enum_str(&OutcomeKind::Merged))
        .bind(efficacy_grace_days as i32)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|row| {
            if row.get::<bool, _>("recurred") {
                FixEfficacy::Recurred
            } else if row.get::<DateTime<Utc>, _>("grace_end") < now {
                FixEfficacy::Confirmed
            } else {
                FixEfficacy::Pending
            }
        }))
    }
}
