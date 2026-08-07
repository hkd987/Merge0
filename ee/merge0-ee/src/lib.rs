//! Merge0 `/ee` — the commercial layer of the open-core split.
//!
//! The PRD ("Distribution & Open Source Strategy", the split table) draws the
//! line: the MIT core under `crates/` is a complete, honest single-tenant
//! product; this crate holds what only makes sense hosted — multi-tenant org
//! management, RBAC, the audit log, usage metering/billing, and the
//! **cross-tenant outcome priors** data service. Nothing under `crates/` may
//! depend on this crate (CLAUDE.md invariant 5); this crate builds *on* the
//! stable `merge0-store` / `merge0-signal` APIs and never reaches around
//! them.
//!
//! Modules:
//!
//! - [`tenants`] — control plane: the `merge0_control` schema, tenant
//!   provisioning/suspension via [`tenants::TenantManager`].
//! - [`rbac`] — roles, actions, and the permission matrix.
//! - [`audit`] — the append-only audit log every mutation writes to.
//! - [`billing`] — the two Phase 2 pricing experiments + usage metering.
//! - [`priors`] — anonymized cross-tenant "what kinds of fixes merge"
//!   aggregation (the hosted-only data service).
//!
//! Conventions: typed errors ([`EeError`]), no panics on input, and `now` is
//! always passed in — no wall-clock reads inside logic.

pub mod audit;
pub mod billing;
pub mod priors;
pub mod rbac;
pub mod tenants;

pub use audit::AuditEntry;
pub use billing::{invoice, usage, Invoice, Pricing, Usage, MARKET_PRICE_CENTS};
pub use priors::{compute_priors, GatePriors, PriorBucket, MIN_PRIOR_ATTEMPTS};
pub use rbac::{allowed, Action, Role};
pub use tenants::{Tenant, TenantManager, CONTROL_SCHEMA};

#[derive(Debug, thiserror::Error)]
pub enum EeError {
    #[error("store error: {0}")]
    Store(#[from] merge0_store::StoreError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("tenant not found: {0}")]
    TenantNotFound(String),
    #[error("tenant suspended: {0}")]
    TenantSuspended(String),
    #[error("forbidden: {email} may not {action} on tenant {tenant_id}")]
    Forbidden {
        tenant_id: String,
        email: String,
        action: String,
    },
    #[error("invalid pricing: {0}")]
    InvalidPricing(String),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, EeError>;

// ---- serialization helpers shared by the modules (same pattern as
// merge0-store: the serde rename is the single source of wire names) ----

pub(crate) fn enum_str<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        other => unreachable!("unit enum must serialize to a JSON string, got {other:?}"),
    }
}

pub(crate) fn enum_parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| EeError::Corrupt(format!("bad enum value {s:?}: {e}")))
}

pub(crate) fn parse_ulid(s: &str) -> Result<ulid::Ulid> {
    ulid::Ulid::from_string(s).map_err(|e| EeError::Corrupt(format!("bad ulid {s:?}: {e}")))
}
