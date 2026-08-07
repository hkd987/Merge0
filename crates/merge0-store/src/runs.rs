//! Dispatches and outcomes: what happened after approval.
//!
//! `dispatches` tracks the runner-side journey of a Work Order (PRD P0-6),
//! including MCP/skill attribution (§5b) and token spend (P2 cost
//! accounting). `outcomes` is outcome memory (P0-8): every PR fate — with
//! revert-as-hard-negative — plus discarded runs and their salvage
//! diagnosis, queryable by fingerprint for `prior_attempts`.

use crate::{enum_parse, enum_str, parse_ulid, Result, StoreError, TenantStore};
use chrono::{DateTime, Utc};
use merge0_signal::{OutcomeKind, OutcomeRef};
use sqlx::Row;
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStatus {
    Dispatched,
    PrOpen,
    Discarded,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchRecord {
    pub report_id: Ulid,
    pub runner_kind: String,
    pub dispatched_at: DateTime<Utc>,
    pub status: DispatchStatus,
    pub pr_url: Option<String>,
    pub branch: Option<String>,
    pub pr_opened_at: Option<DateTime<Utc>>,
    pub discard_reason: Option<String>,
    /// Failed-run salvage: the agent's root-cause investigation (PRD §5).
    pub diagnosis: Option<String>,
    pub tokens_spent: Option<u64>,
    /// MCP/skill attribution for this run (PRD §5b).
    pub extensions: Option<serde_json::Value>,
}

impl TenantStore {
    pub async fn record_dispatch(
        &self,
        report_id: Ulid,
        runner_kind: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let sql = format!(
            "INSERT INTO {t} (report_id, runner_kind, dispatched_at, status)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (report_id) DO UPDATE SET
                 runner_kind = EXCLUDED.runner_kind,
                 dispatched_at = EXCLUDED.dispatched_at,
                 status = EXCLUDED.status,
                 pr_url = NULL, branch = NULL, pr_opened_at = NULL,
                 discard_reason = NULL, diagnosis = NULL",
            t = self.table("dispatches")
        );
        sqlx::query(&sql)
            .bind(report_id.to_string())
            .bind(runner_kind)
            .bind(now)
            .bind(enum_str(&DispatchStatus::Dispatched))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Runner callback: a test-passing PR was opened.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_pr_opened(
        &self,
        report_id: Ulid,
        pr_url: &str,
        branch: &str,
        now: DateTime<Utc>,
        tokens_spent: Option<u64>,
        extensions: Option<serde_json::Value>,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET status = $2, pr_url = $3, branch = $4, pr_opened_at = $5,
                            tokens_spent = $6, extensions = $7
             WHERE report_id = $1",
            t = self.table("dispatches")
        );
        let updated = sqlx::query(&sql)
            .bind(report_id.to_string())
            .bind(enum_str(&DispatchStatus::PrOpen))
            .bind(pr_url)
            .bind(branch)
            .bind(now)
            .bind(tokens_spent.map(|n| n as i64))
            .bind(extensions)
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "dispatch for report {report_id}"
            )));
        }
        Ok(())
    }

    /// Runner callback: the run self-discarded (repair budget exhausted or
    /// diff budget exceeded). Also writes the discarded outcome + salvage.
    pub async fn record_discard(
        &self,
        report_id: Ulid,
        reason: &str,
        diagnosis: &str,
        now: DateTime<Utc>,
        tokens_spent: Option<u64>,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET status = $2, discard_reason = $3, diagnosis = $4, tokens_spent = $5
             WHERE report_id = $1",
            t = self.table("dispatches")
        );
        let updated = sqlx::query(&sql)
            .bind(report_id.to_string())
            .bind(enum_str(&DispatchStatus::Discarded))
            .bind(reason)
            .bind(diagnosis)
            .bind(tokens_spent.map(|n| n as i64))
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "dispatch for report {report_id}"
            )));
        }
        self.record_outcome(
            report_id,
            OutcomeKind::Discarded,
            None,
            now,
            Some(&format!("{reason}: {diagnosis}")),
            tokens_spent,
        )
        .await
    }

    pub async fn dispatch(&self, report_id: Ulid) -> Result<Option<DispatchRecord>> {
        let sql = format!(
            "SELECT * FROM {t} WHERE report_id = $1",
            t = self.table("dispatches")
        );
        let row = sqlx::query(&sql)
            .bind(report_id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.map(|row| {
            Ok(DispatchRecord {
                report_id: parse_ulid(row.get("report_id"))?,
                runner_kind: row.get("runner_kind"),
                dispatched_at: row.get("dispatched_at"),
                status: enum_parse(row.get("status"))?,
                pr_url: row.get("pr_url"),
                branch: row.get("branch"),
                pr_opened_at: row.get("pr_opened_at"),
                discard_reason: row.get("discard_reason"),
                diagnosis: row.get("diagnosis"),
                tokens_spent: row.get::<Option<i64>, _>("tokens_spent").map(|n| n as u64),
                extensions: row.get("extensions"),
            })
        })
        .transpose()
    }

    /// Record the merge commit SHA when the PR merges — the key revert
    /// detection maps back through (PRD P0-8).
    pub async fn record_merge_sha(&self, report_id: Ulid, merge_sha: &str) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET merge_sha = $2 WHERE report_id = $1",
            t = self.table("dispatches")
        );
        let updated = sqlx::query(&sql)
            .bind(report_id.to_string())
            .bind(merge_sha)
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "dispatch for report {report_id}"
            )));
        }
        Ok(())
    }

    /// Find the report whose merged PR produced this commit SHA (prefix
    /// match, since revert messages may carry abbreviated SHAs).
    pub async fn report_for_merge_sha(&self, sha: &str) -> Result<Option<Ulid>> {
        if sha.len() < 7 {
            return Ok(None); // refuse ambiguous short prefixes
        }
        let sql = format!(
            "SELECT report_id FROM {t}
             WHERE merge_sha IS NOT NULL
               AND (merge_sha LIKE $1 || '%' OR $1 LIKE merge_sha || '%')",
            t = self.table("dispatches")
        );
        let row = sqlx::query(&sql)
            .bind(sha)
            .fetch_optional(self.pool())
            .await?;
        row.map(|r| parse_ulid(r.get("report_id"))).transpose()
    }

    /// Find the report whose dispatch opened a given PR — the webhook
    /// handler's lookup when a PR merges/closes.
    pub async fn report_for_pr(&self, pr_url: &str) -> Result<Option<Ulid>> {
        let sql = format!(
            "SELECT report_id FROM {t} WHERE pr_url = $1",
            t = self.table("dispatches")
        );
        let row = sqlx::query(&sql)
            .bind(pr_url)
            .fetch_optional(self.pool())
            .await?;
        row.map(|r| parse_ulid(r.get("report_id"))).transpose()
    }

    /// Append to outcome memory (PRD P0-8).
    pub async fn record_outcome(
        &self,
        report_id: Ulid,
        kind: OutcomeKind,
        pr_url: Option<&str>,
        occurred_at: DateTime<Utc>,
        note: Option<&str>,
        tokens_spent: Option<u64>,
    ) -> Result<()> {
        let sql = format!(
            "INSERT INTO {t} (id, report_id, kind, pr_url, occurred_at, note, tokens_spent)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            t = self.table("outcomes")
        );
        sqlx::query(&sql)
            .bind(Ulid::new().to_string())
            .bind(report_id.to_string())
            .bind(enum_str(&kind))
            .bind(pr_url)
            .bind(occurred_at)
            .bind(note)
            .bind(tokens_spent.map(|n| n as i64))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Outcome history for a fingerprint — "we tried this in March and it was
    /// reverted". Feeds `WorkOrder::prior_attempts` and hardening targeting.
    pub async fn outcomes_for_fingerprint(&self, fingerprint: &str) -> Result<Vec<OutcomeRef>> {
        let sql = format!(
            "SELECT o.report_id, o.kind, o.occurred_at, o.note
             FROM {outcomes} o
             JOIN {report_signals} rs ON rs.report_id = o.report_id
             WHERE rs.fingerprint = $1
             ORDER BY o.occurred_at DESC",
            outcomes = self.table("outcomes"),
            report_signals = self.table("report_signals"),
        );
        let rows = sqlx::query(&sql)
            .bind(fingerprint)
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|row| {
                Ok(OutcomeRef {
                    // Work orders are 1:1 with reports; the report id is the
                    // work-order identity (see crate docs).
                    work_order_id: parse_ulid(row.get("report_id"))?,
                    outcome: enum_parse(row.get("kind"))?,
                    occurred_at: row.get("occurred_at"),
                    note: row.get("note"),
                })
            })
            .collect()
    }

    pub async fn outcomes_for_report(&self, report_id: Ulid) -> Result<Vec<OutcomeRef>> {
        let sql = format!(
            "SELECT report_id, kind, occurred_at, note FROM {t}
             WHERE report_id = $1 ORDER BY occurred_at DESC",
            t = self.table("outcomes")
        );
        let rows = sqlx::query(&sql)
            .bind(report_id.to_string())
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|row| {
                Ok(OutcomeRef {
                    work_order_id: parse_ulid(row.get("report_id"))?,
                    outcome: enum_parse(row.get("kind"))?,
                    occurred_at: row.get("occurred_at"),
                    note: row.get("note"),
                })
            })
            .collect()
    }

    // ---- releases (release context, PRD P0-4) ----

    pub async fn upsert_release(
        &self,
        version: &str,
        sha: Option<&str>,
        released_at: DateTime<Utc>,
        notes: Option<&str>,
    ) -> Result<()> {
        let sql = format!(
            "INSERT INTO {t} (version, sha, released_at, notes) VALUES ($1,$2,$3,$4)
             ON CONFLICT (version) DO UPDATE SET
                 sha = EXCLUDED.sha, released_at = EXCLUDED.released_at, notes = EXCLUDED.notes",
            t = self.table("releases")
        );
        sqlx::query(&sql)
            .bind(version)
            .bind(sha)
            .bind(released_at)
            .bind(notes)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Releases ordered oldest→newest: the deploy timeline.
    pub async fn releases(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        let sql = format!(
            "SELECT version, released_at FROM {t} ORDER BY released_at ASC",
            t = self.table("releases")
        );
        let rows = sqlx::query(&sql).fetch_all(self.pool()).await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("version"), r.get("released_at")))
            .collect())
    }
}
