//! Credential broker for non-Actions runners (PRD §5a, runner-side P2).
//!
//! In v1 (BYO Actions) the workflow's ephemeral `GITHUB_TOKEN` covers git
//! auth and Merge0 never touches it. On customer VMs and other CI there is
//! no such ambient token, so the broker fills the gap while preserving the
//! §5a invariant: **the agent never sees a credential that outlives its run
//! or exceeds its Work Order's repo scope.** Concretely:
//!
//! - A runner authenticates with a **per-tenant runner key** (compared in
//!   constant time via [`constant_time_eq`]).
//! - Credentials exist only against a **grant** registered at dispatch
//!   approval, binding one Work Order to exactly one repo. Requests for any
//!   other repo, unknown Work Orders, or an already-consumed grant are
//!   denied — each denial is a distinct [`BrokerError`] variant so audit
//!   logs stay precise.
//! - Grants are **single-use**: one credential per approved dispatch.
//! - TTL is **capped at 10 minutes** ([`max_ttl`]) regardless of what the
//!   runner asks for.
//! - The token itself is a [`SecretToken`] whose `Debug`/`Display` print
//!   `[REDACTED]`; the raw value is reachable only through
//!   [`SecretToken::expose_for_credential_helper`], and it is handed to git
//!   via the [`credential_helper`] wire protocol — so it never enters the
//!   agent transcript, prompt context, or logs (the opaque-handle principle).
//!
//! No wall-clock reads: `request_credentials` takes `now` so expiry logic is
//! deterministic under test. The GitHub-App-backed [`TokenMinter`] lives
//! with the GitHub integration; this crate ships [`FakeMinter`] for tests.

pub mod credential_helper;

use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::fmt;

/// Hard ceiling on credential lifetime (PRD §5a: "~10-minute TTL").
pub fn max_ttl() -> Duration {
    Duration::minutes(10)
}

/// A bearer token that refuses to be printed.
///
/// Both `Debug` and `Display` render `[REDACTED]` so the raw value cannot
/// leak through logging, error formatting, or agent transcripts by accident.
/// There is deliberately no `Serialize` impl either. The single escape hatch
/// is [`Self::expose_for_credential_helper`].
#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Expose the raw token value.
    ///
    /// Named for its one legitimate consumer: the git credential helper
    /// response ([`credential_helper::format_response`]), which git reads
    /// over a pipe and keeps out of process arguments, transcripts, and
    /// logs. Any other call site should be treated as a leak in review —
    /// the loud name exists so misuse cannot hide behind a bland `as_str`.
    pub fn expose_for_credential_helper(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Every way a broker request can fail — one variant per denial cause so
/// callers and audit logs can distinguish them without string matching.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    /// The presented runner key matches no registered per-tenant key.
    #[error("runner key not recognized")]
    InvalidRunnerKey,
    /// No grant was registered for this Work Order at dispatch approval.
    #[error("no grant registered for work order {0}")]
    NoGrant(String),
    /// A grant exists but covers a different repo than requested.
    #[error("grant for work order {work_order_id} covers {granted_repo}, not {requested_repo}")]
    RepoMismatch {
        work_order_id: String,
        granted_repo: String,
        requested_repo: String,
    },
    /// The grant was already used — grants are single-use.
    #[error("grant for work order {0} already consumed")]
    GrantConsumed(String),
    /// The upstream minter (GitHub App) failed to produce a token.
    #[error("token minting failed: {0}")]
    Mint(String),
    /// A git credential helper request was not valid wire format.
    #[error("malformed credential helper request: {0}")]
    MalformedCredentialRequest(String),
}

/// A freshly minted token plus the lifetime the minter actually granted
/// (which may be shorter than asked; it is never allowed to be longer than
/// the broker's cap when turned into a [`Credential`]).
pub struct MintedToken {
    pub token: SecretToken,
    pub ttl: Duration,
}

/// Mints repo-scoped tokens. The production impl (GitHub App installation
/// tokens) lives with the GitHub integration crate; the broker only depends
/// on this interface.
pub trait TokenMinter {
    fn mint(&self, repo: &str, ttl: Duration) -> Result<MintedToken, BrokerError>;
}

/// Deterministic minter for tests: token value encodes the repo so tests
/// can assert scoping without real credentials.
#[derive(Default)]
pub struct FakeMinter;

impl TokenMinter for FakeMinter {
    fn mint(&self, repo: &str, ttl: Duration) -> Result<MintedToken, BrokerError> {
        Ok(MintedToken {
            token: SecretToken::new(format!("fake-token-{}", repo.replace('/', "-"))),
            ttl,
        })
    }
}

/// What the runner receives: an opaque token and its expiry.
#[derive(Debug)]
pub struct Credential {
    pub token: SecretToken,
    pub expires_at: DateTime<Utc>,
}

struct Grant {
    repo: String,
    consumed: bool,
}

/// The broker itself: per-tenant runner keys plus single-use Work Order
/// grants, in front of a [`TokenMinter`].
pub struct Broker<M: TokenMinter> {
    minter: M,
    runner_keys: Vec<String>,
    grants: HashMap<String, Grant>,
}

impl<M: TokenMinter> Broker<M> {
    pub fn new(minter: M) -> Self {
        Self {
            minter,
            runner_keys: Vec::new(),
            grants: HashMap::new(),
        }
    }

    /// Register a per-tenant runner key. Presented keys are checked against
    /// every registered key in constant time per key.
    pub fn add_runner_key(&mut self, key: impl Into<String>) {
        self.runner_keys.push(key.into());
    }

    /// Record that an approved Work Order may draw exactly one credential
    /// for exactly one repo. Called at dispatch approval (PRD §5a: "broker
    /// denies requests for repos not referenced by an approved Work Order").
    /// Re-registering the same Work Order id resets the grant.
    pub fn register_grant(&mut self, work_order_id: impl Into<String>, repo: impl Into<String>) {
        self.grants.insert(
            work_order_id.into(),
            Grant {
                repo: repo.into(),
                consumed: false,
            },
        );
    }

    /// Convenience: register a grant straight from an approved
    /// [`WorkOrder`](merge0_signal::WorkOrder). Work Orders are keyed by
    /// their originating report (one Work Order per Report), so the report
    /// id serves as the Work Order id.
    pub fn register_grant_for(&mut self, work_order: &merge0_signal::WorkOrder) {
        self.register_grant(work_order.report_id.to_string(), work_order.repo.clone());
    }

    /// Exchange a runner key + Work Order reference for a short-lived,
    /// single-repo credential.
    ///
    /// Check order: authentication first (an unauthenticated caller learns
    /// nothing about which grants exist), then grant existence, repo scope,
    /// and single-use state. `requested_ttl` is clamped to
    /// `0..=`[`max_ttl`]; the effective lifetime is the shorter of the
    /// clamped request and what the minter actually granted. The grant is
    /// consumed only after minting succeeds, so a transient mint failure
    /// does not burn the dispatch.
    pub fn request_credentials(
        &mut self,
        runner_key: &str,
        work_order_id: &str,
        repo: &str,
        requested_ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Credential, BrokerError> {
        if !self.runner_key_matches(runner_key) {
            return Err(BrokerError::InvalidRunnerKey);
        }
        let grant = self
            .grants
            .get(work_order_id)
            .ok_or_else(|| BrokerError::NoGrant(work_order_id.to_string()))?;
        if grant.repo != repo {
            return Err(BrokerError::RepoMismatch {
                work_order_id: work_order_id.to_string(),
                granted_repo: grant.repo.clone(),
                requested_repo: repo.to_string(),
            });
        }
        if grant.consumed {
            return Err(BrokerError::GrantConsumed(work_order_id.to_string()));
        }

        let ttl = requested_ttl.clamp(Duration::zero(), max_ttl());
        let minted = self.minter.mint(repo, ttl)?;
        if let Some(grant) = self.grants.get_mut(work_order_id) {
            grant.consumed = true;
        }
        let effective_ttl = minted.ttl.clamp(Duration::zero(), ttl);
        Ok(Credential {
            token: minted.token,
            expires_at: now + effective_ttl,
        })
    }

    /// Check the presented key against every registered key without early
    /// exit, so timing does not reveal which (if any) key matched.
    /// Does this key belong to a registered runner?
    ///
    /// Public so a caller can authenticate *before* doing any work on an
    /// unauthenticated request body — [`Self::request_credentials`] checks
    /// the same thing, but only after the caller has already parsed.
    pub fn authenticates(&self, presented: &str) -> bool {
        self.runner_key_matches(presented)
    }

    fn runner_key_matches(&self, presented: &str) -> bool {
        let mut matched = false;
        for key in &self.runner_keys {
            matched |= constant_time_eq(key.as_bytes(), presented.as_bytes());
        }
        matched
    }
}

/// Byte-wise constant-time equality: XOR differences accumulated with
/// bitwise OR, so runtime is independent of where the first mismatch is.
/// Lengths are compared up front — leaking key *length* is accepted; keys
/// are fixed-size random strings, so length carries no secret.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const RUNNER_KEY: &str = "tenant-key-0123456789abcdef";
    const WO: &str = "wo-01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const REPO: &str = "example-org/example-repo";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    fn broker_with_grant() -> Broker<FakeMinter> {
        let mut broker = Broker::new(FakeMinter);
        broker.add_runner_key(RUNNER_KEY);
        broker.register_grant(WO, REPO);
        broker
    }

    #[test]
    fn happy_path_grant_to_credential_within_ttl_cap() {
        let mut broker = broker_with_grant();
        let cred = broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(5), now())
            .unwrap();
        assert_eq!(cred.expires_at, now() + Duration::minutes(5));
        assert!(cred.expires_at <= now() + max_ttl());
        assert_eq!(
            cred.token.expose_for_credential_helper(),
            "fake-token-example-org-example-repo"
        );
    }

    #[test]
    fn ttl_is_capped_at_ten_minutes() {
        let mut broker = broker_with_grant();
        let cred = broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::hours(1), now())
            .unwrap();
        assert_eq!(cred.expires_at, now() + Duration::minutes(10));
    }

    #[test]
    fn negative_requested_ttl_is_clamped_to_zero() {
        let mut broker = broker_with_grant();
        let cred = broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(-5), now())
            .unwrap();
        assert_eq!(cred.expires_at, now());
    }

    #[test]
    fn wrong_runner_key_is_denied() {
        let mut broker = broker_with_grant();
        let err = broker
            .request_credentials("wrong-key", WO, REPO, Duration::minutes(5), now())
            .unwrap_err();
        assert!(matches!(err, BrokerError::InvalidRunnerKey));
    }

    #[test]
    fn unknown_work_order_is_denied() {
        let mut broker = broker_with_grant();
        let err = broker
            .request_credentials(RUNNER_KEY, "wo-unknown", REPO, Duration::minutes(5), now())
            .unwrap_err();
        assert!(matches!(err, BrokerError::NoGrant(id) if id == "wo-unknown"));
    }

    #[test]
    fn repo_outside_grant_scope_is_denied() {
        let mut broker = broker_with_grant();
        let err = broker
            .request_credentials(
                RUNNER_KEY,
                WO,
                "example-org/other-repo",
                Duration::minutes(5),
                now(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            BrokerError::RepoMismatch { requested_repo, .. } if requested_repo == "example-org/other-repo"
        ));
    }

    #[test]
    fn grants_are_single_use() {
        let mut broker = broker_with_grant();
        broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(5), now())
            .unwrap();
        let err = broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(5), now())
            .unwrap_err();
        assert!(matches!(err, BrokerError::GrantConsumed(id) if id == WO));
    }

    #[test]
    fn mint_failure_does_not_consume_the_grant() {
        struct FlakyMinter {
            fail_first: std::cell::Cell<bool>,
        }
        impl TokenMinter for FlakyMinter {
            fn mint(&self, repo: &str, ttl: Duration) -> Result<MintedToken, BrokerError> {
                if self.fail_first.replace(false) {
                    Err(BrokerError::Mint("upstream unavailable".into()))
                } else {
                    FakeMinter.mint(repo, ttl)
                }
            }
        }
        let mut broker = Broker::new(FlakyMinter {
            fail_first: std::cell::Cell::new(true),
        });
        broker.add_runner_key(RUNNER_KEY);
        broker.register_grant(WO, REPO);

        let err = broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(5), now())
            .unwrap_err();
        assert!(matches!(err, BrokerError::Mint(_)));
        // The retry succeeds: the failed mint did not burn the dispatch.
        broker
            .request_credentials(RUNNER_KEY, WO, REPO, Duration::minutes(5), now())
            .unwrap();
    }

    #[test]
    fn register_grant_for_uses_work_order_identity() {
        let order = merge0_signal::WorkOrder {
            report_id: ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            repo: REPO.into(),
            summary: "Fix null district crash".into(),
            evidence: vec![],
            repro: "Open the sync panel with no linked district".into(),
            suspect_change: None,
            success_criteria: "Regression test passes".into(),
            constraints: "Single concern".into(),
            prior_attempts: vec![],
            diff_budget: Default::default(),
            confidence: Default::default(),
        };
        let mut broker = Broker::new(FakeMinter);
        broker.add_runner_key(RUNNER_KEY);
        broker.register_grant_for(&order);
        broker
            .request_credentials(
                RUNNER_KEY,
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                REPO,
                Duration::minutes(5),
                now(),
            )
            .unwrap();
    }

    #[test]
    fn secret_token_is_redacted_in_debug_and_display() {
        let token = SecretToken::new("ghs_do_not_print_me");
        assert_eq!(format!("{token:?}"), "[REDACTED]");
        assert_eq!(token.to_string(), "[REDACTED]");
        // The whole Credential's Debug output is safe to log too.
        let cred = Credential {
            token: SecretToken::new("ghs_do_not_print_me"),
            expires_at: now(),
        };
        let debug = format!("{cred:?}");
        assert!(!debug.contains("ghs_do_not_print_me"));
        assert!(debug.contains("[REDACTED]"));
        // The explicit escape hatch still works.
        assert_eq!(token.expose_for_credential_helper(), "ghs_do_not_print_me");
    }

    #[test]
    fn constant_time_eq_semantics() {
        assert!(constant_time_eq(b"same-key", b"same-key"));
        assert!(!constant_time_eq(b"same-key", b"same-kez"));
        assert!(!constant_time_eq(b"short", b"longer-key"));
        assert!(constant_time_eq(b"", b""));
    }
}
