//! Report lifecycle: creation by clustering, gate decisions, human verdicts,
//! and the fingerprint-history queries that feed outcome memory into triage.

use crate::signals::row_to_signal;
use crate::{enum_parse, enum_str, parse_ulid, Result, StoreError, TenantStore};
use chrono::{DateTime, Utc};
use merge0_signal::{DismissReason, GateDecision, Report, ReportStatus, Signal, WorkOrder};
use sqlx::postgres::PgRow;
use sqlx::Row;
use ulid::Ulid;

impl TenantStore {
    /// Persist a freshly assembled Report and its signal memberships.
    pub async fn insert_report(&self, report: &Report) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let sql = format!(
            "INSERT INTO {t} (id, kind, title, summary, severity, evidence, suspect_release,
                              affected_count, status, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
            t = self.table("reports")
        );
        sqlx::query(&sql)
            .bind(report.id.to_string())
            .bind(enum_str(&report.kind))
            .bind(&report.title)
            .bind(&report.summary)
            .bind(enum_str(&report.severity))
            .bind(serde_json::to_value(&report.evidence).expect("evidence serializes"))
            .bind(&report.suspect_release)
            .bind(report.affected_count.map(|n| n as i64))
            .bind(enum_str(&report.status))
            .bind(report.created_at)
            .execute(&mut *tx)
            .await?;

        if report.signal_ids.len() != report.fingerprints.len() {
            return Err(StoreError::Corrupt(
                "report signal_ids and fingerprints must be parallel".into(),
            ));
        }
        let member_sql = format!(
            "INSERT INTO {t} (report_id, signal_id, fingerprint) VALUES ($1,$2,$3)",
            t = self.table("report_signals")
        );
        for (signal_id, fingerprint) in report.signal_ids.iter().zip(&report.fingerprints) {
            sqlx::query(&member_sql)
                .bind(report.id.to_string())
                .bind(signal_id.to_string())
                .bind(fingerprint)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_report(&self, id: Ulid) -> Result<Report> {
        let sql = format!("SELECT * FROM {t} WHERE id = $1", t = self.table("reports"));
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("report {id}")))?;
        self.hydrate_report(&row).await
    }

    pub async fn list_reports(&self, status: Option<ReportStatus>) -> Result<Vec<Report>> {
        let sql = match status {
            Some(_) => format!(
                "SELECT * FROM {t} WHERE status = $1 ORDER BY created_at DESC",
                t = self.table("reports")
            ),
            None => format!(
                "SELECT * FROM {t} ORDER BY created_at DESC",
                t = self.table("reports")
            ),
        };
        let query = match status {
            Some(s) => sqlx::query(&sql).bind(enum_str(&s)),
            None => sqlx::query(&sql),
        };
        let rows = query.fetch_all(self.pool()).await?;
        let mut reports = Vec::with_capacity(rows.len());
        for row in &rows {
            reports.push(self.hydrate_report(row).await?);
        }
        Ok(reports)
    }

    /// Record the gate's decision and move the report to the matching status.
    pub async fn set_gate_decision(&self, id: Ulid, decision: &GateDecision) -> Result<()> {
        let status = match decision {
            GateDecision::Work { .. } => ReportStatus::AwaitingReview,
            GateDecision::Skip { .. } => ReportStatus::Skipped,
        };
        let mut tx = self.pool().begin().await?;
        let sql = format!(
            "UPDATE {t} SET gate_decision = $2, status = $3 WHERE id = $1",
            t = self.table("reports")
        );
        let updated = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(serde_json::to_value(decision).expect("decision serializes"))
            .bind(enum_str(&status))
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("report {id}")));
        }
        if let GateDecision::Work { work_order } = decision {
            let wo_sql = format!(
                "INSERT INTO {t} (report_id, payload) VALUES ($1, $2)
                 ON CONFLICT (report_id) DO UPDATE SET payload = EXCLUDED.payload",
                t = self.table("work_orders")
            );
            sqlx::query(&wo_sql)
                .bind(id.to_string())
                .bind(serde_json::to_value(work_order).expect("work order serializes"))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn gate_decision(&self, id: Ulid) -> Result<Option<GateDecision>> {
        let sql = format!(
            "SELECT gate_decision FROM {t} WHERE id = $1",
            t = self.table("reports")
        );
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("report {id}")))?;
        let value: Option<serde_json::Value> = row.get("gate_decision");
        value
            .map(|v| {
                serde_json::from_value(v)
                    .map_err(|e| StoreError::Corrupt(format!("gate_decision column: {e}")))
            })
            .transpose()
    }

    pub async fn work_order(&self, report_id: Ulid) -> Result<Option<WorkOrder>> {
        let sql = format!(
            "SELECT payload FROM {t} WHERE report_id = $1",
            t = self.table("work_orders")
        );
        let row = sqlx::query(&sql)
            .bind(report_id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.map(|r| {
            serde_json::from_value(r.get::<serde_json::Value, _>("payload"))
                .map_err(|e| StoreError::Corrupt(format!("work_order payload: {e}")))
        })
        .transpose()
    }

    /// Human approval (inbox or Slack). Stamps `decided_at`.
    pub async fn approve_report(&self, id: Ulid, now: DateTime<Utc>) -> Result<()> {
        self.verdict(id, ReportStatus::Approved, None, now).await
    }

    /// Human dismissal with a structured reason. Stamps `decided_at`.
    pub async fn dismiss_report(
        &self,
        id: Ulid,
        reason: DismissReason,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.verdict(id, ReportStatus::Dismissed, Some(reason), now)
            .await
    }

    /// Terminal handoff for an Opportunity Report: stores the evidence brief.
    pub async fn hand_off_report(&self, id: Ulid, brief: &str, now: DateTime<Utc>) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET status = $2, handoff_brief = $3, decided_at = $4 WHERE id = $1",
            t = self.table("reports")
        );
        let updated = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(enum_str(&ReportStatus::HandedOff))
            .bind(brief)
            .bind(now)
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("report {id}")));
        }
        Ok(())
    }

    pub async fn handoff_brief(&self, id: Ulid) -> Result<Option<String>> {
        let sql = format!(
            "SELECT handoff_brief FROM {t} WHERE id = $1",
            t = self.table("reports")
        );
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("report {id}")))?;
        Ok(row.get("handoff_brief"))
    }

    /// Pipeline status advance without a human verdict (dispatched, pr_open,
    /// completed).
    pub async fn set_report_status(&self, id: Ulid, status: ReportStatus) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET status = $2 WHERE id = $1",
            t = self.table("reports")
        );
        let updated = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(enum_str(&status))
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("report {id}")));
        }
        Ok(())
    }

    /// Reports (any status) that contain a fingerprint — recurrence history
    /// for the gate's `prior_attempts` and the hardening pass targeting.
    pub async fn reports_containing_fingerprint(&self, fingerprint: &str) -> Result<Vec<Report>> {
        let sql = format!(
            "SELECT r.* FROM {reports} r
             JOIN {report_signals} rs ON rs.report_id = r.id
             WHERE rs.fingerprint = $1
             ORDER BY r.created_at DESC",
            reports = self.table("reports"),
            report_signals = self.table("report_signals"),
        );
        let rows = sqlx::query(&sql)
            .bind(fingerprint)
            .fetch_all(self.pool())
            .await?;
        let mut reports = Vec::with_capacity(rows.len());
        for row in &rows {
            reports.push(self.hydrate_report(row).await?);
        }
        Ok(reports)
    }

    /// Was any report containing this fingerprint dismissed with the given
    /// reason? Feeds the Opportunity classifier ("users repeatedly colliding
    /// with the design").
    pub async fn fingerprint_dismissed_as(
        &self,
        fingerprint: &str,
        reason: DismissReason,
    ) -> Result<bool> {
        let sql = format!(
            "SELECT EXISTS (
                 SELECT 1 FROM {reports} r
                 JOIN {report_signals} rs ON rs.report_id = r.id
                 WHERE rs.fingerprint = $1 AND r.dismiss_reason = $2
             )",
            reports = self.table("reports"),
            report_signals = self.table("report_signals"),
        );
        let exists: bool = sqlx::query_scalar(&sql)
            .bind(fingerprint)
            .bind(enum_str(&reason))
            .fetch_one(self.pool())
            .await?;
        Ok(exists)
    }

    /// Was any report containing a signal at this `url_path` dismissed with
    /// the given reason? Recurring design collisions arrive as *new*
    /// fingerprints (a fresh ticket, a fresh session) at the *same*
    /// location — location is the recurrence key fingerprints can't provide.
    pub async fn url_path_dismissed_as(
        &self,
        url_path: &str,
        reason: DismissReason,
    ) -> Result<bool> {
        let sql = format!(
            "SELECT EXISTS (
                 SELECT 1 FROM {reports} r
                 JOIN {report_signals} rs ON rs.report_id = r.id
                 JOIN {signals} s ON s.id = rs.signal_id
                 WHERE s.join_keys->>'url_path' = $1 AND r.dismiss_reason = $2
             )",
            reports = self.table("reports"),
            report_signals = self.table("report_signals"),
            signals = self.table("signals"),
        );
        let exists: bool = sqlx::query_scalar(&sql)
            .bind(url_path)
            .bind(enum_str(&reason))
            .fetch_one(self.pool())
            .await?;
        Ok(exists)
    }

    /// Member signals of a report, fully hydrated.
    pub async fn report_signals(&self, report_id: Ulid) -> Result<Vec<Signal>> {
        let sql = format!(
            "SELECT s.* FROM {signals} s
             JOIN {report_signals} rs ON rs.signal_id = s.id
             WHERE rs.report_id = $1
             ORDER BY s.last_seen DESC",
            signals = self.table("signals"),
            report_signals = self.table("report_signals"),
        );
        let rows = sqlx::query(&sql)
            .bind(report_id.to_string())
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(row_to_signal).collect()
    }

    async fn verdict(
        &self,
        id: Ulid,
        status: ReportStatus,
        reason: Option<DismissReason>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET status = $2, dismiss_reason = $3, decided_at = $4 WHERE id = $1",
            t = self.table("reports")
        );
        let updated = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(enum_str(&status))
            .bind(reason.map(|r| enum_str(&r)))
            .bind(now)
            .execute(self.pool())
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("report {id}")));
        }
        Ok(())
    }

    async fn hydrate_report(&self, row: &PgRow) -> Result<Report> {
        let id: String = row.get("id");
        let members_sql = format!(
            "SELECT signal_id, fingerprint FROM {t} WHERE report_id = $1 ORDER BY signal_id",
            t = self.table("report_signals")
        );
        let members = sqlx::query(&members_sql)
            .bind(&id)
            .fetch_all(self.pool())
            .await?;
        let mut signal_ids = Vec::with_capacity(members.len());
        let mut fingerprints = Vec::with_capacity(members.len());
        for member in &members {
            signal_ids.push(parse_ulid(member.get("signal_id"))?);
            fingerprints.push(member.get("fingerprint"));
        }
        let evidence: serde_json::Value = row.get("evidence");
        Ok(Report {
            id: parse_ulid(&id)?,
            kind: enum_parse(row.get("kind"))?,
            title: row.get("title"),
            summary: row.get("summary"),
            severity: enum_parse(row.get("severity"))?,
            evidence: serde_json::from_value(evidence)
                .map_err(|e| StoreError::Corrupt(format!("evidence column: {e}")))?,
            signal_ids,
            fingerprints,
            suspect_release: row.get("suspect_release"),
            affected_count: row
                .get::<Option<i64>, _>("affected_count")
                .map(|n| n as u64),
            status: enum_parse(row.get("status"))?,
            created_at: row.get("created_at"),
        })
    }
}
