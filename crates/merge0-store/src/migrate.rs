//! Tenant schema provisioning: an ordered migration-step registry.
//!
//! Each step is `(version, DDL statements)`. `provision` applies, in order,
//! every step strictly greater than the recorded version, then records the
//! new version — so a tenant provisioned at v1 picks up v2's `ALTER`s, and
//! a fresh tenant runs every step from scratch. Within one release the
//! registry is immutable (PRD §5d: the executor changes only through
//! versioned releases); a release adds steps, never edits shipped ones.

use crate::Result;
use sqlx::PgPool;

/// The registry. Append-only across releases.
fn steps(schema: &str) -> Vec<(i32, Vec<String>)> {
    vec![
        (1, ddl_v1(schema)),
        (2, ddl_v2(schema)),
        (3, ddl_v3(schema)),
    ]
}

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
    let current = current.unwrap_or(0);

    let mut applied = current;
    for (version, statements) in steps(schema) {
        if version <= current {
            continue;
        }
        for statement in statements {
            sqlx::query(&statement).execute(pool).await?;
        }
        applied = version;
    }
    if applied != current {
        sqlx::query(&format!("DELETE FROM \"{schema}\".schema_meta"))
            .execute(pool)
            .await?;
        sqlx::query(&format!(
            "INSERT INTO \"{schema}\".schema_meta (version) VALUES ($1)"
        ))
        .bind(applied)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// v3 — the market-gap pass (schema v0.4 + autonomy/escalation audit):
/// - `signals.delegated`: ticket explicitly handed to Merge0 via a tracker
///   label; prioritized by triage
/// - `dispatches.dispatched_by`: who pulled the trigger (`human`, `slack`,
///   or `auto`) — the autonomy dial's audit trail
/// - `reports.dismissal_affected_count`: affected-count snapshot at dismissal
///   time, the baseline for escalation re-opens
/// - `triage_runs`: per-run token ledger, the substrate of the rolling
///   24h spend budget
fn ddl_v3(schema: &str) -> Vec<String> {
    let s = schema;
    vec![
        format!(
            "ALTER TABLE \"{s}\".signals
                 ADD COLUMN IF NOT EXISTS delegated BOOLEAN NOT NULL DEFAULT FALSE"
        ),
        format!(
            "ALTER TABLE \"{s}\".dispatches
                 ADD COLUMN IF NOT EXISTS dispatched_by TEXT NOT NULL DEFAULT 'human'"
        ),
        format!(
            "ALTER TABLE \"{s}\".reports
                 ADD COLUMN IF NOT EXISTS dismissal_affected_count BIGINT"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".triage_runs (
                 id TEXT PRIMARY KEY,
                 started_at TIMESTAMPTZ NOT NULL,
                 tokens_used BIGINT NOT NULL,
                 budget_exhausted BOOLEAN NOT NULL DEFAULT FALSE
             )"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_triage_runs_started
                 ON \"{s}\".triage_runs (started_at)"
        ),
    ]
}

/// v2 — idempotency + ingestion state:
/// - one outcome row per (report, kind): GitHub redelivers webhooks
///   at-least-once and inflated outcome counts corrupt the Phase 0 metric
/// - `webhook_deliveries`: `x-github-delivery` dedupe
/// - `fetch_state`: per-source poll cursors for the fetch layer
fn ddl_v2(schema: &str) -> Vec<String> {
    let s = schema;
    vec![
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_outcomes_report_kind
                 ON \"{s}\".outcomes (report_id, kind)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".webhook_deliveries (
                 delivery_id TEXT PRIMARY KEY,
                 received_at TIMESTAMPTZ NOT NULL
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".fetch_state (
                 source TEXT PRIMARY KEY,
                 cursor TEXT,
                 last_run TIMESTAMPTZ
             )"
        ),
    ]
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
