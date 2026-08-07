//! Tenant schema provisioning.
//!
//! Idempotent DDL executed inside the tenant's schema. A `schema_meta` table
//! records the applied store version so future releases can migrate forward;
//! within one release the DDL is immutable (PRD §5d: the executor changes
//! only through versioned releases).

use crate::Result;
use sqlx::PgPool;

/// Bump when the DDL below changes shape; `provision` applies steps
/// strictly greater than the recorded version.
const STORE_VERSION: i32 = 1;

pub(crate) async fn provision(pool: &PgPool, schema: &str) -> Result<()> {
    // `schema` is validated by the caller (Store::tenant) before we get here.
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS \"{schema}\".schema_meta (
             version INT NOT NULL
         )"
    ))
    .execute(pool)
    .await?;

    let current: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT version FROM \"{schema}\".schema_meta LIMIT 1"
    ))
    .fetch_optional(pool)
    .await?;

    if current.unwrap_or(0) < STORE_VERSION {
        for statement in ddl_v1(schema) {
            sqlx::query(&statement).execute(pool).await?;
        }
        sqlx::query(&format!("DELETE FROM \"{schema}\".schema_meta"))
            .execute(pool)
            .await?;
        sqlx::query(&format!(
            "INSERT INTO \"{schema}\".schema_meta (version) VALUES ($1)"
        ))
        .bind(STORE_VERSION)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn ddl_v1(schema: &str) -> Vec<String> {
    let s = schema;
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".signals (
                 id TEXT PRIMARY KEY,
                 source TEXT NOT NULL,
                 source_ref TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body TEXT NOT NULL,
                 evidence JSONB NOT NULL,
                 fingerprint TEXT NOT NULL UNIQUE,
                 join_keys JSONB NOT NULL,
                 affected_count BIGINT,
                 first_seen TIMESTAMPTZ NOT NULL,
                 last_seen TIMESTAMPTZ NOT NULL,
                 raw JSONB NOT NULL,
                 ingested_at TIMESTAMPTZ NOT NULL DEFAULT now()
             )"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_signals_last_seen
                 ON \"{s}\".signals (last_seen)"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_signals_stack_hash
                 ON \"{s}\".signals ((join_keys->>'stack_hash'))"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".reports (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 evidence JSONB NOT NULL,
                 suspect_release TEXT,
                 affected_count BIGINT,
                 status TEXT NOT NULL,
                 dismiss_reason TEXT,
                 gate_decision JSONB,
                 handoff_brief TEXT,
                 created_at TIMESTAMPTZ NOT NULL,
                 decided_at TIMESTAMPTZ
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".report_signals (
                 report_id TEXT NOT NULL REFERENCES \"{s}\".reports(id) ON DELETE CASCADE,
                 signal_id TEXT NOT NULL REFERENCES \"{s}\".signals(id) ON DELETE CASCADE,
                 fingerprint TEXT NOT NULL,
                 PRIMARY KEY (report_id, signal_id)
             )"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_report_signals_fingerprint
                 ON \"{s}\".report_signals (fingerprint)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".work_orders (
                 report_id TEXT PRIMARY KEY REFERENCES \"{s}\".reports(id) ON DELETE CASCADE,
                 payload JSONB NOT NULL,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".dispatches (
                 report_id TEXT PRIMARY KEY REFERENCES \"{s}\".reports(id) ON DELETE CASCADE,
                 runner_kind TEXT NOT NULL,
                 dispatched_at TIMESTAMPTZ NOT NULL,
                 status TEXT NOT NULL,
                 pr_url TEXT,
                 branch TEXT,
                 pr_opened_at TIMESTAMPTZ,
                 discard_reason TEXT,
                 diagnosis TEXT,
                 tokens_spent BIGINT,
                 extensions JSONB,
                 merge_sha TEXT
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".outcomes (
                 id TEXT PRIMARY KEY,
                 report_id TEXT NOT NULL REFERENCES \"{s}\".reports(id) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 pr_url TEXT,
                 occurred_at TIMESTAMPTZ NOT NULL,
                 note TEXT,
                 tokens_spent BIGINT
             )"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_outcomes_report
                 ON \"{s}\".outcomes (report_id)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".releases (
                 version TEXT PRIMARY KEY,
                 sha TEXT,
                 released_at TIMESTAMPTZ NOT NULL,
                 notes TEXT
             )"
        ),
    ]
}
