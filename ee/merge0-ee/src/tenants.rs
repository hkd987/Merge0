//! Control plane: multi-tenant org management (PRD split table — "Multi-tenant
//! org management, SSO/SAML, RBAC, audit log" is `/ee` + hosted).
//!
//! Cross-tenant state lives in one dedicated schema, [`CONTROL_SCHEMA`],
//! provisioned idempotently in the style of `merge0-store`'s `migrate.rs`.
//! Per-tenant data stays in per-tenant schemas provisioned exclusively
//! through [`Store::tenant`] — this crate never hand-rolls tenant DDL, so
//! `merge0-store`'s schema-name validation remains the single injection
//! boundary for tenant-controlled names. The control schema name itself is a
//! compile-time constant and therefore safe to interpolate.

use crate::{parse_ulid, EeError, Result};
use chrono::{DateTime, Utc};
use merge0_store::{Store, TenantStore};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use ulid::Ulid;

/// The dedicated schema holding cross-tenant control-plane tables.
pub const CONTROL_SCHEMA: &str = "merge0_control";

/// One hosted organization. `schema_name` is derived from the ULID id at
/// creation time (`tenant_<id lowercase>`) and always passes
/// `merge0-store`'s schema-name validation by construction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tenant {
    pub id: Ulid,
    pub name: String,
    pub schema_name: String,
    pub plan: String,
    pub created_at: DateTime<Utc>,
    pub suspended: bool,
}

/// The hosted control plane: wraps a [`Store`] and owns the
/// [`CONTROL_SCHEMA`] tables. Every mutation writes an audit entry.
#[derive(Clone)]
pub struct TenantManager {
    pool: PgPool,
    store: Store,
}

impl TenantManager {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Self::from_pool(pool).await
    }

    /// Build on an existing pool, provisioning the control schema
    /// idempotently.
    pub async fn from_pool(pool: PgPool) -> Result<Self> {
        provision_control(&pool).await?;
        Ok(TenantManager {
            store: Store::from_pool(pool.clone()),
            pool,
        })
    }

    /// The underlying store — for operator tooling (e.g. offboarding via
    /// `Store::drop_tenant`).
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn table(name: &str) -> String {
        format!("\"{CONTROL_SCHEMA}\".{name}")
    }

    /// Create a tenant: generate its id, provision its schema through
    /// [`Store::tenant`], record the row, and audit `tenant.created`.
    pub async fn create_tenant(
        &self,
        name: &str,
        plan: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<Tenant> {
        let id = Ulid::generate();
        let schema_name = format!("tenant_{}", id.to_string().to_lowercase());
        // Provision through the store — its validation + DDL, never ours.
        self.store.tenant(&schema_name).await?;
        let sql = format!(
            "INSERT INTO {t} (id, name, schema_name, plan, created_at, suspended)
             VALUES ($1,$2,$3,$4,$5,false)",
            t = Self::table("tenants")
        );
        sqlx::query(&sql)
            .bind(id.to_string())
            .bind(name)
            .bind(&schema_name)
            .bind(plan)
            .bind(now)
            .execute(&self.pool)
            .await?;
        self.record(
            Some(&id.to_string()),
            actor,
            "tenant.created",
            Some(name),
            Some(serde_json::json!({ "plan": plan, "schema_name": schema_name })),
            now,
        )
        .await?;
        Ok(Tenant {
            id,
            name: name.to_string(),
            schema_name,
            plan: plan.to_string(),
            created_at: now,
            suspended: false,
        })
    }

    pub async fn get_tenant(&self, id: Ulid) -> Result<Tenant> {
        let sql = format!(
            "SELECT * FROM {t} WHERE id = $1",
            t = Self::table("tenants")
        );
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| EeError::TenantNotFound(id.to_string()))?;
        row_to_tenant(&row)
    }

    /// All tenants (including suspended ones), oldest first.
    pub async fn list_tenants(&self) -> Result<Vec<Tenant>> {
        let sql = format!(
            "SELECT * FROM {t} ORDER BY created_at ASC, id ASC",
            t = Self::table("tenants")
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(row_to_tenant).collect()
    }

    /// Suspend a tenant: its [`TenantStore`] becomes unreachable through
    /// [`TenantManager::tenant_store`] and it is excluded from cross-tenant
    /// priors. Audited as `tenant.suspended`.
    pub async fn suspend_tenant(&self, id: Ulid, actor: &str, now: DateTime<Utc>) -> Result<()> {
        self.set_suspended(id, true, actor, "tenant.suspended", now)
            .await
    }

    /// Reverse a suspension. Audited as `tenant.resumed`.
    pub async fn resume_tenant(&self, id: Ulid, actor: &str, now: DateTime<Utc>) -> Result<()> {
        self.set_suspended(id, false, actor, "tenant.resumed", now)
            .await
    }

    async fn set_suspended(
        &self,
        id: Ulid,
        suspended: bool,
        actor: &str,
        action: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE {t} SET suspended = $2 WHERE id = $1",
            t = Self::table("tenants")
        );
        let updated = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(suspended)
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(EeError::TenantNotFound(id.to_string()));
        }
        self.record(Some(&id.to_string()), actor, action, None, None, now)
            .await
    }

    /// Open the tenant's per-tenant store. Suspended tenants are refused with
    /// [`EeError::TenantSuspended`].
    pub async fn tenant_store(&self, tenant: &Tenant) -> Result<TenantStore> {
        if tenant.suspended {
            return Err(EeError::TenantSuspended(tenant.id.to_string()));
        }
        Ok(self.store.tenant(&tenant.schema_name).await?)
    }
}

fn row_to_tenant(row: &sqlx::postgres::PgRow) -> Result<Tenant> {
    Ok(Tenant {
        id: parse_ulid(row.get("id"))?,
        name: row.get("name"),
        schema_name: row.get("schema_name"),
        plan: row.get("plan"),
        created_at: row.get("created_at"),
        suspended: row.get("suspended"),
    })
}

/// Idempotent control-schema DDL, mirroring `merge0-store`'s `migrate.rs`
/// style. A `schema_meta` row records the applied version so future releases
/// can migrate forward.
const CONTROL_VERSION: i32 = 1;

async fn provision_control(pool: &PgPool) -> Result<()> {
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{CONTROL_SCHEMA}\""))
        .execute(pool)
        .await?;
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS \"{CONTROL_SCHEMA}\".schema_meta (
             version INT NOT NULL
         )"
    ))
    .execute(pool)
    .await?;

    let current: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT version FROM \"{CONTROL_SCHEMA}\".schema_meta LIMIT 1"
    ))
    .fetch_optional(pool)
    .await?;

    if current.unwrap_or(0) < CONTROL_VERSION {
        for statement in ddl_v1() {
            sqlx::query(&statement).execute(pool).await?;
        }
        sqlx::query(&format!("DELETE FROM \"{CONTROL_SCHEMA}\".schema_meta"))
            .execute(pool)
            .await?;
        sqlx::query(&format!(
            "INSERT INTO \"{CONTROL_SCHEMA}\".schema_meta (version) VALUES ($1)"
        ))
        .bind(CONTROL_VERSION)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn ddl_v1() -> Vec<String> {
    let s = CONTROL_SCHEMA;
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".tenants (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 schema_name TEXT NOT NULL UNIQUE,
                 plan TEXT NOT NULL,
                 created_at TIMESTAMPTZ NOT NULL,
                 suspended BOOL NOT NULL DEFAULT false
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".members (
                 tenant_id TEXT NOT NULL REFERENCES \"{s}\".tenants(id) ON DELETE CASCADE,
                 email TEXT NOT NULL,
                 role TEXT NOT NULL,
                 PRIMARY KEY (tenant_id, email)
             )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{s}\".audit_log (
                 id TEXT PRIMARY KEY,
                 tenant_id TEXT,
                 actor TEXT NOT NULL,
                 action TEXT NOT NULL,
                 subject TEXT,
                 at TIMESTAMPTZ NOT NULL,
                 details JSONB
             )"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_audit_tenant_at
                 ON \"{s}\".audit_log (tenant_id, at DESC)"
        ),
    ]
}
