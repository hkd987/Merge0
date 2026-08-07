//! The adapter contract and the golden-payload conformance harness.
//!
//! Adapters are the only place vendor payloads exist. The core never imports
//! vendor types (PRD §Architecture rule); everything downstream consumes
//! normalized [`merge0_signal::Signal`]s. Conformance is testable by
//! construction: golden vendor payloads in, expected Signals out — the
//! harness in [`testing`] is the same one community adapters are expected to
//! use.

use merge0_signal::{Signal, Source};
use serde::Deserialize;

pub mod testing;

/// What an adapter receives: a vendor API response wrapped by the fetch layer
/// in an envelope that records which endpoint produced it plus any
/// project-level context (base URLs for deep links, etc.).
///
/// The `payload` is the vendor response **verbatim** — the envelope exists so
/// adapters dispatch on an explicit endpoint name instead of sniffing payload
/// shapes.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// Adapter-defined endpoint name, e.g. `error_tracking_issues`.
    pub endpoint: String,
    /// Adapter-defined context; each adapter deserializes its own typed form.
    #[serde(default)]
    pub context: serde_json::Value,
    /// The vendor API response, verbatim.
    pub payload: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("malformed input: {0}")]
    Malformed(String),
    #[error("unsupported endpoint: {0}")]
    UnsupportedEndpoint(String),
}

/// Vendor payload in, normalized Signals out.
///
/// Implementations must be pure with respect to the input (no I/O): the fetch
/// layer does the talking to vendor APIs, adapters only normalize. That is
/// what makes golden-payload conformance testing possible.
/// (`Send + Sync` because the server holds adapters across await points.)
pub trait Adapter: Send + Sync {
    /// The source every emitted Signal must carry.
    fn source(&self) -> Source;

    /// Normalize one envelope (see [`Envelope`]) into zero or more Signals.
    ///
    /// Must not panic on malformed input — return [`AdapterError::Malformed`]
    /// with a reason instead. Unknown vendor fields are ignored by the typed
    /// parse but preserved verbatim in each Signal's `raw`.
    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError>;
}
