//! Postgres store (PRD §3, "Stack & Deployment"): schema-per-tenant.
//!
//! A [`Store`] wraps the connection pool; [`Store::tenant`] provisions (or
//! opens) one tenant's schema and returns a [`TenantStore`] whose queries are
//! all schema-qualified. Tenant schema names are strictly validated
//! (`[a-z_][a-z0-9_]*`, max 63 chars) before ever being interpolated into
//! SQL — that validation is the injection boundary, and it is tested.
//!
//! Storage layout per tenant: `signals` (deduped by fingerprint),
//! `reports` + `report_signals`, `work_orders` (1:1 with reports —
//! `OutcomeRef::work_order_id` therefore carries the report id),
//! `dispatches`, `outcomes` (outcome memory, PRD P0-8), `releases`
//! (release context, P0-4). Telemetry (P0-10) is computed by SQL counts
//! fed through the pure [`TelemetrySnapshot::from_counts`].

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use ulid::Ulid;

mod migrate;
mod reports;
mod runs;
mod signals;
mod telemetry;

pub use runs::{DispatchRecord, DispatchStatus};
pub use signals::IngestOutcome;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid tenant schema name: {0:?}")]
    InvalidSchemaName(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Root handle: one per process.
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Store { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Store { pool }
    }

    /// Open (provisioning if needed) one tenant's schema.
    pub async fn tenant(&self, schema: &str) -> Result<TenantStore> {
        validate_schema_name(schema)?;
        migrate::provision(&self.pool, schema).await?;
        Ok(TenantStore {
            pool: self.pool.clone(),
            schema: schema.to_string(),
        })
    }

    /// Drop a tenant schema and everything in it. Destructive; used by tests
    /// and tenant offboarding.
    pub async fn drop_tenant(&self, schema: &str) -> Result<()> {
        validate_schema_name(schema)?;
        sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// All operations for a single tenant. Cheap to clone.
#[derive(Clone)]
pub struct TenantStore {
    pool: PgPool,
    schema: String,
}

impl TenantStore {
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) fn table(&self, name: &str) -> String {
        format!("\"{}\".{name}", self.schema)
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// The injection boundary for tenant-controlled schema names.
fn validate_schema_name(schema: &str) -> Result<()> {
    let valid = !schema.is_empty()
        && schema.len() <= 63
        && schema
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && schema
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidSchemaName(schema.to_string()))
    }
}

// ---- serialization helpers shared by the modules ----

/// Serialize a unit-variant enum to its wire string (the serde rename), so
/// the store never duplicates the schema crate's naming tables.
pub(crate) fn enum_str<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        other => unreachable!("unit enum must serialize to a JSON string, got {other:?}"),
    }
}

pub(crate) fn enum_parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| StoreError::Corrupt(format!("bad enum value {s:?}: {e}")))
}

pub(crate) fn parse_ulid(s: &str) -> Result<Ulid> {
    Ulid::from_string(s).map_err(|e| StoreError::Corrupt(format!("bad ulid {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_signal::{DismissReason, OutcomeKind, ReportStatus};

    #[test]
    fn schema_name_validation_blocks_injection() {
        assert!(validate_schema_name("tenant_chalk").is_ok());
        assert!(validate_schema_name("_x1").is_ok());
        for bad in [
            "",
            "Tenant",
            "t-enant",
            "t.enant",
            "t\"; DROP SCHEMA public; --",
            "1tenant",
            &"a".repeat(64),
        ] {
            assert!(validate_schema_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn enum_str_round_trips_schema_enums() {
        assert_eq!(enum_str(&ReportStatus::AwaitingReview), "awaiting_review");
        assert_eq!(enum_str(&OutcomeKind::Reverted), "reverted");
        assert_eq!(
            enum_str(&DismissReason::IntendedBehavior),
            "intended_behavior"
        );
        let status: ReportStatus = enum_parse("pr_open").unwrap();
        assert_eq!(status, ReportStatus::PrOpen);
        assert!(enum_parse::<ReportStatus>("bogus").is_err());
    }
}

// Re-exported for integration tests and downstream crates that need the raw
// types without importing sqlx themselves.
pub mod prelude {
    pub use super::{IngestOutcome, Store, StoreError, TenantStore};
    pub use merge0_signal::{
        DismissReason, GateDecision, OutcomeKind, OutcomeRef, Report, ReportKind, ReportStatus,
        Signal, TelemetrySnapshot, WorkOrder,
    };
}
