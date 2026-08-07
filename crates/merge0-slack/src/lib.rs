//! The Slack surface (PRD §6, §6a item 2).
//!
//! Slack is the second-highest-traffic reviewer surface after the PR
//! description, so every message here is built to a testable constraint:
//! **one report or PR per message**, decision-ready inline (severity,
//! affected count, one-line evidence summary, approve/dismiss actions),
//! with a deep link into the web inbox as *fallback, never requirement*.
//! That constraint is encoded in the builder signatures — each takes exactly
//! one [`Report`](merge0_signal::Report) — and asserted again in tests.
//!
//! Design decisions:
//!
//! - **Builders are pure functions** returning Block Kit JSON. No I/O, no
//!   wall-clock reads, so they are trivially golden-testable and reusable by
//!   both the webhook sink and any future `chat.postMessage` client.
//! - **Delivery is behind [`SlackSink`]** so the server can swap the real
//!   webhook for [`RecordingSink`] in tests and dry-run mode.
//! - **Dismissals are structured** (PRD §6): the dismiss select offers exactly
//!   the four [`DismissReason`](merge0_signal::DismissReason) values, and
//!   [`parse_interaction`] maps them back to the typed enum so verdicts feed
//!   outcome memory without free-text parsing.
//! - **The weekly digest ages pending PRs** and flags anything older than
//!   [`STALE_PR_AGE_DAYS`] — reviewer abandonment is the PRD's #1 tracked
//!   risk, and stale-PR aging in the digest is its stated mitigation.

mod interaction;
mod message;
mod signature;
mod sink;

pub use interaction::{parse_interaction, SlackVerdict, Verdict};
pub use message::{
    pr_ready_message, report_message, weekly_digest, ACTION_APPROVE, ACTION_DISMISS,
    DISMISS_REASONS, STALE_PR_AGE_DAYS,
};
pub use signature::{verify_slack_signature, SIGNATURE_MAX_AGE_SECS};
pub use sink::{RecordingSink, SlackSink, WebhookSink};

/// Typed errors for the Slack surface — untrusted interaction payloads must
/// never panic, they must land in one of these.
#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    /// The HTTP request to the webhook failed (network / TLS / build error).
    #[error("webhook request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Slack answered the webhook POST with a non-success status.
    #[error("webhook returned HTTP status {0}")]
    WebhookStatus(u16),
    /// The interactivity payload was structurally invalid.
    #[error("invalid interaction payload: {0}")]
    InvalidPayload(String),
    /// The payload parsed but carried an action we did not emit.
    #[error("unknown action_id: {0}")]
    UnknownAction(String),
    /// A dismiss action carried a reason outside the four structured values.
    #[error("unknown dismiss reason: {0}")]
    UnknownDismissReason(String),
}
