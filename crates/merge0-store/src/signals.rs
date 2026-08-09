//! Signal persistence: fingerprint-deduped upsert (PRD P0-3's substrate) and
//! the queries scouts and clustering run.

use crate::{enum_parse, enum_str, parse_ulid, Result, StoreError, TenantStore};
use chrono::{DateTime, Utc};
use merge0_signal::Signal;
use sqlx::postgres::PgRow;
use sqlx::Row;

/// Whether an ingested Signal was new or refreshed an existing fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    Inserted,
    Updated,
}

impl TenantStore {
    /// Insert or refresh a Signal, deduping on `fingerprint`: the same
    /// underlying defect re-ingested widens the seen-window and refreshes
    /// the volatile facts (severity, counts, evidence, raw) instead of
    /// creating a duplicate row.
    pub async fn upsert_signal(&self, signal: &Signal) -> Result<IngestOutcome> {
        let sql = format!(
            "INSERT INTO {t} (id, source, source_ref, kind, severity, title, body, evidence,
                              fingerprint, join_keys, affected_count, delegated,
                              first_seen, last_seen, raw)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (fingerprint) DO UPDATE SET
                 severity = EXCLUDED.severity,
                 title = EXCLUDED.title,
                 body = EXCLUDED.body,
                 evidence = EXCLUDED.evidence,
                 join_keys = EXCLUDED.join_keys,
                 affected_count = EXCLUDED.affected_count,
                 delegated = EXCLUDED.delegated,
                 first_seen = LEAST({t}.first_seen, EXCLUDED.first_seen),
                 last_seen = GREATEST({t}.last_seen, EXCLUDED.last_seen),
                 raw = EXCLUDED.raw
             RETURNING (xmax = 0) AS inserted",
            t = self.table("signals")
        );
        let row = sqlx::query(&sql)
            .bind(signal.id.to_string())
            .bind(enum_str(&signal.source))
            .bind(&signal.source_ref)
            .bind(enum_str(&signal.kind))
            .bind(enum_str(&signal.severity))
            .bind(&signal.title)
            .bind(&signal.body)
            .bind(serde_json::to_value(&signal.evidence).expect("evidence serializes"))
            .bind(&signal.fingerprint)
            .bind(serde_json::to_value(&signal.join_keys).expect("join_keys serializes"))
            .bind(signal.affected_count.map(|n| n as i64))
            .bind(signal.delegated)
            .bind(signal.first_seen)
            .bind(signal.last_seen)
            .bind(&signal.raw)
            .fetch_one(self.pool())
            .await?;
        let inserted: bool = row.get("inserted");
        Ok(if inserted {
            IngestOutcome::Inserted
        } else {
            IngestOutcome::Updated
        })
    }

    /// Signals not yet assigned to any report — clustering's input.
    pub async fn unassigned_signals(&self) -> Result<Vec<Signal>> {
        let sql = format!(
            "SELECT s.* FROM {signals} s
             WHERE NOT EXISTS (
                 SELECT 1 FROM {report_signals} rs WHERE rs.signal_id = s.id
             )
             ORDER BY s.last_seen DESC",
            signals = self.table("signals"),
            report_signals = self.table("report_signals"),
        );
        let rows = sqlx::query(&sql).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_signal).collect()
    }

    /// Signals seen since a cutoff — the scout query substrate.
    pub async fn signals_since(&self, cutoff: DateTime<Utc>) -> Result<Vec<Signal>> {
        let sql = format!(
            "SELECT * FROM {t} WHERE last_seen >= $1 ORDER BY last_seen DESC",
            t = self.table("signals")
        );
        let rows = sqlx::query(&sql)
            .bind(cutoff)
            .fetch_all(self.pool())
            .await?;
        rows.iter().map(row_to_signal).collect()
    }

    pub async fn signal_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Signal>> {
        let sql = format!(
            "SELECT * FROM {t} WHERE fingerprint = $1",
            t = self.table("signals")
        );
        let row = sqlx::query(&sql)
            .bind(fingerprint)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(row_to_signal).transpose()
    }
}

pub(crate) fn row_to_signal(row: &PgRow) -> Result<Signal> {
    let evidence: serde_json::Value = row.get("evidence");
    let join_keys: serde_json::Value = row.get("join_keys");
    Ok(Signal {
        id: parse_ulid(row.get("id"))?,
        source: enum_parse(row.get("source"))?,
        source_ref: row.get("source_ref"),
        kind: enum_parse(row.get("kind"))?,
        severity: enum_parse(row.get("severity"))?,
        title: row.get("title"),
        body: row.get("body"),
        evidence: serde_json::from_value(evidence)
            .map_err(|e| StoreError::Corrupt(format!("evidence column: {e}")))?,
        fingerprint: row.get("fingerprint"),
        join_keys: serde_json::from_value(join_keys)
            .map_err(|e| StoreError::Corrupt(format!("join_keys column: {e}")))?,
        affected_count: row
            .get::<Option<i64>, _>("affected_count")
            .map(|n| n as u64),
        delegated: row.get("delegated"),
        first_seen: row.get("first_seen"),
        last_seen: row.get("last_seen"),
        raw: row.get("raw"),
    })
}
