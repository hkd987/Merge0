//! GitHub integration (PRD §5a, P0-9, P0-11).
//!
//! - [`api`]: the `GitHubApi` trait every component talks to, plus
//!   `RestGitHub` (real) and `FakeGitHub` (tests).
//! - [`auth`]: GitHub App authentication — short-lived installation tokens
//!   only; no PATs, no user OAuth tokens exist anywhere in the system.
//! - [`webhook`]: signature verification and event parsing, including
//!   revert detection (P0-8's hard negative).
//! - [`safety`]: onboarding verification — branch protection + required CI
//!   confirmed before any dispatch is allowed (P0-9).

pub mod api;
pub mod auth;
pub mod codeowners;
pub mod safety;
pub mod webhook;

pub use api::{FakeGitHub, GitHubApi, GitHubError, PrInfo, ReleaseInfo, RepoRef};

pub type Result<T> = std::result::Result<T, GitHubError>;
