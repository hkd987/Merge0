//! The fetch layer (PRD §1): the component that talks to vendor APIs and
//! feeds the pure adapters.
//!
//! A production audit found the PRD's fetch layer was never built — adapters
//! existed but nothing produced their envelopes from live vendor APIs. This
//! crate closes that gap:
//!
//! - [`Fetcher`] — one implementation per source ([`pollers`]), each turning
//!   a vendor API response into the `{endpoint, context, payload}` envelopes
//!   the adapters consume (vendor response **verbatim** in `payload`).
//! - [`run_fetch`] — the orchestration: load the per-source cursor from the
//!   store, fetch, normalize through the fetcher's adapter, upsert Signals,
//!   persist the next cursor.
//! - [`config`] — typed `config/sources.toml`; secrets are env-var *names*
//!   only, resolved at fetcher construction (CLAUDE.md rule 4).
//! - [`webhooks`] — pure signature-verification and envelope-building
//!   helpers for the vendors' native webhooks; the server mounts them.
//!
//! No wall-clock inside logic: `now` is always passed in (the reqwest client
//! timeout is the one deliberate exception).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use merge0_store::{IngestOutcome, TenantStore};
use std::fmt;

pub mod config;
pub mod pollers;
pub mod webhooks;

pub use config::{build_fetchers, SourcesConfig};

/// Every way a fetch can fail — typed so `run_all` callers and telemetry can
/// distinguish an unreachable vendor from a misconfiguration from vendor
/// drift without string matching.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Network-level failure (DNS, TLS, timeout, malformed response body).
    #[error("transport error: {0}")]
    Transport(String),
    /// The vendor API answered with a non-2xx status.
    #[error("vendor API returned {status}: {message}")]
    Api { status: u16, message: String },
    /// Bad or incomplete configuration (including a missing secret env var —
    /// the message names the variable, never a value).
    #[error("config error: {0}")]
    Config(String),
    /// The adapter rejected an envelope this fetcher built. This fails the
    /// whole run on purpose: it means the vendor's response shape drifted
    /// from what the adapter understands, and skipping silently would rot
    /// the signal stream.
    #[error("adapter rejected envelope: {0}")]
    Adapter(String),
    /// Persistence failure while loading cursors or upserting Signals.
    #[error("store error: {0}")]
    Store(#[from] merge0_store::StoreError),
}

/// One fetch round's raw product: adapter envelopes plus the cursor to
/// persist for the next round (`None` clears the cursor).
#[derive(Debug, Clone)]
pub struct FetchBatch {
    /// `{endpoint, context, payload}` envelopes, vendor response verbatim in
    /// `payload` (see [`merge0_adapters::Envelope`]).
    pub envelopes: Vec<serde_json::Value>,
    pub next_cursor: Option<String>,
}

/// What one [`run_fetch`] accomplished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub source: String,
    pub envelopes: usize,
    pub inserted: u64,
    pub updated: u64,
}

/// One source's fetch strategy: talk to the vendor, emit adapter envelopes.
///
/// Implementations do the I/O; the paired [`Adapter`](merge0_adapters::Adapter)
/// stays pure. `cursor` is whatever this fetcher returned as
/// [`FetchBatch::next_cursor`] last round (opaque to the orchestration).
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Stable name used as the cursor key in the store (e.g. `"sentry"`).
    fn source_name(&self) -> &'static str;

    /// The adapter that normalizes this fetcher's envelopes.
    fn adapter(&self) -> Box<dyn merge0_adapters::Adapter>;

    async fn fetch(
        &self,
        cursor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FetchBatch, FetchError>;
}

/// One full fetch round for one source: cursor → vendor → adapter → store.
///
/// An [`AdapterError`](merge0_adapters::AdapterError) on any envelope fails
/// the run with [`FetchError::Adapter`] — vendor drift surfaces loudly
/// instead of being skipped silently. The cursor is only advanced after
/// every Signal of the round upserted.
pub async fn run_fetch(
    fetcher: &dyn Fetcher,
    store: &TenantStore,
    now: DateTime<Utc>,
) -> Result<FetchOutcome, FetchError> {
    let source = fetcher.source_name();
    let cursor = store.fetch_cursor(source).await?;
    let batch = fetcher.fetch(cursor.as_deref(), now).await?;
    let adapter = fetcher.adapter();

    let mut inserted = 0u64;
    let mut updated = 0u64;
    for envelope in &batch.envelopes {
        let signals = adapter
            .normalize(envelope)
            .map_err(|e| FetchError::Adapter(format!("{source}: {e}")))?;
        for signal in &signals {
            match store.upsert_signal(signal).await? {
                IngestOutcome::Inserted => inserted += 1,
                IngestOutcome::Updated => updated += 1,
            }
        }
    }

    store
        .set_fetch_cursor(source, batch.next_cursor.as_deref(), now)
        .await?;

    Ok(FetchOutcome {
        source: source.to_string(),
        envelopes: batch.envelopes.len(),
        inserted,
        updated,
    })
}

/// Run every fetcher; one source's failure never aborts the batch. Returns
/// each source's outcome in input order.
pub async fn run_all(
    fetchers: &[Box<dyn Fetcher>],
    store: &TenantStore,
    now: DateTime<Utc>,
) -> Vec<(String, Result<FetchOutcome, FetchError>)> {
    let mut results = Vec::with_capacity(fetchers.len());
    for fetcher in fetchers {
        let result = run_fetch(fetcher.as_ref(), store, now).await;
        results.push((fetcher.source_name().to_string(), result));
    }
    results
}

/// A vendor credential that refuses to be printed (mirrors
/// `merge0-broker`'s `SecretToken`).
///
/// `Debug` and `Display` render `[REDACTED]`; there is deliberately no
/// `Serialize` impl. The single escape hatch is
/// [`Self::expose_for_auth_header`] — any other call site is a leak in
/// review.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Resolve from the environment by variable *name* (the only way config
    /// references secrets). Missing or empty → [`FetchError::Config`] naming
    /// the variable, never any value.
    pub fn from_env(var_name: &str) -> Result<Self, FetchError> {
        match std::env::var(var_name) {
            Ok(value) if !value.is_empty() => Ok(Secret(value)),
            _ => Err(FetchError::Config(format!(
                "environment variable {var_name} is not set"
            ))),
        }
    }

    /// Expose the raw value. Named for its one legitimate consumer: building
    /// a vendor auth header on an outgoing request.
    pub fn expose_for_auth_header(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_prints_its_value() {
        let secret = Secret::new("phx_do_not_print_me");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(secret.expose_for_auth_header(), "phx_do_not_print_me");
    }

    #[test]
    fn secret_from_env_names_the_missing_variable() {
        let err = Secret::from_env("MERGE0_TEST_LIB_UNSET_VAR").unwrap_err();
        match err {
            FetchError::Config(message) => {
                assert!(message.contains("MERGE0_TEST_LIB_UNSET_VAR"), "{message}")
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
