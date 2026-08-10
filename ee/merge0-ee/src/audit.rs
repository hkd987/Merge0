//! Audit log (PRD split table: audit log is `/ee` + hosted).
//!
//! Append-only. Every mutating [`TenantManager`] / billing operation writes
//! exactly one entry via [`TenantManager::record`]; reads never do. Entries
//! deliberately survive tenant deletion (no FK on `tenant_id`) — an audit
//! trail that vanishes with its subject is not an audit trail.

use crate::{parse_ulid, Result, TenantManager};
use chrono::{DateTime, Utc};
use sqlx::Row;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub id: Ulid,
    pub tenant_id: Option<String>,
    pub actor: String,
    pub action: String,
    pub subject: Option<String>,
    pub at: DateTime<Utc>,
    pub details: Option<serde_json::Value>,
}

impl TenantManager {
    /// Append one audit entry. `tenant_id` is `None` for control-plane-wide
    /// events that concern no single tenant.
    pub async fn record(
        &self,
        tenant_id: Option<&str>,
        actor: &str,
        action: &str,
        subject: Option<&str>,
        details: Option<serde_json::Value>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let sql = format!(
            "INSERT INTO {t} (id, tenant_id, actor, action, subject, at, details)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            t = Self::table("audit_log")
        );
        sqlx::query(&sql)
            .bind(Ulid::generate().to_string())
            .bind(tenant_id)
            .bind(actor)
            .bind(action)
            .bind(subject)
            .bind(now)
            .bind(details)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Newest-first audit entries for one tenant. `id` (a ULID, so
    /// creation-ordered) breaks ties between same-timestamp entries.
    pub async fn entries_for_tenant(&self, tenant_id: &str, limit: u32) -> Result<Vec<AuditEntry>> {
        let sql = format!(
            "SELECT * FROM {t} WHERE tenant_id = $1
             ORDER BY at DESC, id DESC LIMIT $2",
            t = Self::table("audit_log")
        );
        let rows = sqlx::query(&sql)
            .bind(tenant_id)
            .bind(i64::from(limit))
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|row| {
                Ok(AuditEntry {
                    id: parse_ulid(row.get("id"))?,
                    tenant_id: row.get("tenant_id"),
                    actor: row.get("actor"),
                    action: row.get("action"),
                    subject: row.get("subject"),
                    at: row.get("at"),
                    details: row.get("details"),
                })
            })
            .collect()
    }
}
