//! GitHub Issues → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `issues` — the GitHub list-issues API response (a bare JSON array of
//!   issue objects), one `ticket` Signal per issue.
//!
//! Envelope context: `{"repo": "owner/name"}` — required; the repo is part of
//! the fingerprint because issue numbers are only unique per repository.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Pull requests are skipped, not errored.** The GitHub issues API
//!   returns pull requests alongside issues (a documented vendor quirk);
//!   any element carrying a `pull_request` key is silently dropped — PRs are
//!   outputs of the loop, not signals into it.
//! - **Severity** comes from label names, matched case-insensitively as
//!   substrings: a label containing `critical` or `p0` → critical, `high` or
//!   `p1` → high, `bug` or `p2` → medium, otherwise low. Substring matching
//!   deliberately catches namespaced labels like `priority: critical`.
//! - **`join_keys.account_id`** is the issue author's `user.login` — the
//!   closest thing a GitHub issue has to a vendor-side account.
//! - **`affected_count`** is `reactions.total_count` when present: reactions
//!   are the best available proxy for how many people care.
//! - **`body`** may be JSON `null` on GitHub; normalized to empty string.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct GithubIssuesAdapter;

/// Typed context for the GitHub Issues envelope.
#[derive(Debug, Deserialize)]
struct Context {
    repo: String,
}

/// A GitHub issue — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Issue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    reactions: Option<Reactions>,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Deserialize)]
struct Reactions {
    total_count: u64,
}

impl Adapter for GithubIssuesAdapter {
    fn source(&self) -> Source {
        Source::Github
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid github context: {e}")))?;

        match envelope.endpoint.as_str() {
            "issues" => {
                let issues = envelope.payload.as_array().ok_or_else(|| {
                    AdapterError::Malformed("issues payload must be a JSON array".into())
                })?;
                issues
                    .iter()
                    // The issues API returns PRs too — skip them (module docs).
                    .filter(|issue| issue.get("pull_request").is_none())
                    .map(|issue| normalize_issue(issue, &context.repo))
                    .collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_issue(raw: &serde_json::Value, repo: &str) -> Result<Signal, AdapterError> {
    let issue: Issue = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid github issue: {e}")))?;
    let number = issue.number.to_string();

    Ok(Signal {
        id: Ulid::new(),
        source: Source::Github,
        source_ref: number.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_labels(&issue.labels),
        title: issue.title.clone(),
        body: issue.body.clone().unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("GitHub issue {repo}#{number}"),
            url: issue.html_url.clone(),
        }],
        fingerprint: fingerprint(Source::Github, &["issue", repo, &number]),
        join_keys: JoinKeys {
            account_id: issue.user.as_ref().map(|u| u.login.clone()),
            ..Default::default()
        },
        affected_count: issue.reactions.as_ref().map(|r| r.total_count),
        first_seen: issue.created_at,
        last_seen: issue.updated_at,
        raw: raw.clone(),
    })
}

/// Label names → severity (see module docs). The highest matching tier wins.
fn severity_from_labels(labels: &[Label]) -> Severity {
    let names: Vec<String> = labels.iter().map(|l| l.name.to_lowercase()).collect();
    let any = |needles: &[&str]| {
        names
            .iter()
            .any(|name| needles.iter().any(|needle| name.contains(needle)))
    };
    if any(&["critical", "p0"]) {
        Severity::Critical
    } else if any(&["high", "p1"]) {
        Severity::High
    } else if any(&["bug", "p2"]) {
        Severity::Medium
    } else {
        Severity::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<Label> {
        names
            .iter()
            .map(|name| Label {
                name: name.to_string(),
            })
            .collect()
    }

    #[test]
    fn label_severity_mapping() {
        assert_eq!(severity_from_labels(&labels(&[])), Severity::Low);
        assert_eq!(
            severity_from_labels(&labels(&["enhancement"])),
            Severity::Low
        );
        assert_eq!(severity_from_labels(&labels(&["bug"])), Severity::Medium);
        assert_eq!(severity_from_labels(&labels(&["P2"])), Severity::Medium);
        assert_eq!(severity_from_labels(&labels(&["P1"])), Severity::High);
        assert_eq!(
            severity_from_labels(&labels(&["priority: high"])),
            Severity::High
        );
        assert_eq!(severity_from_labels(&labels(&["p0"])), Severity::Critical);
        assert_eq!(
            severity_from_labels(&labels(&["Critical"])),
            Severity::Critical
        );
        // Highest tier wins regardless of label order.
        assert_eq!(
            severity_from_labels(&labels(&["bug", "critical"])),
            Severity::Critical
        );
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "repo": "acme/chalk" },
            "payload": payload,
        })
    }

    fn minimal_issue() -> serde_json::Value {
        serde_json::json!({
            "number": 118,
            "title": "Roster import drops students with no grade band",
            "html_url": "https://github.com/acme/chalk/issues/118",
            "created_at": "2026-08-05T14:00:00Z",
            "updated_at": "2026-08-06T09:00:00Z"
        })
    }

    #[test]
    fn pull_requests_are_skipped_not_errored() {
        let mut pr = minimal_issue();
        pr["number"] = serde_json::json!(119);
        pr["pull_request"] = serde_json::json!({
            "url": "https://api.github.com/repos/acme/chalk/pulls/119"
        });
        let signals = GithubIssuesAdapter
            .normalize(&envelope(
                "issues",
                serde_json::json!([minimal_issue(), pr]),
            ))
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "118");
    }

    #[test]
    fn fingerprint_stable_across_payload_variants_but_repo_scoped() {
        // Same issue number, different labels/updated_at → same fingerprint.
        let mut updated = minimal_issue();
        updated["labels"] = serde_json::json!([{ "name": "critical" }]);
        updated["updated_at"] = serde_json::json!("2026-08-07T09:00:00Z");
        let a = &GithubIssuesAdapter
            .normalize(&envelope("issues", serde_json::json!([minimal_issue()])))
            .unwrap()[0];
        let b = &GithubIssuesAdapter
            .normalize(&envelope("issues", serde_json::json!([updated])))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.severity, b.severity);

        // Same number in a different repo → different fingerprint.
        let other_repo = serde_json::json!({
            "endpoint": "issues",
            "context": { "repo": "acme/other" },
            "payload": [minimal_issue()],
        });
        let c = &GithubIssuesAdapter.normalize(&other_repo).unwrap()[0];
        assert_ne!(a.fingerprint, c.fingerprint);
    }

    #[test]
    fn account_id_and_affected_count_when_present() {
        let mut issue = minimal_issue();
        issue["user"] = serde_json::json!({ "login": "chalk-teacher" });
        issue["reactions"] = serde_json::json!({ "total_count": 7 });
        let signals = GithubIssuesAdapter
            .normalize(&envelope("issues", serde_json::json!([issue])))
            .unwrap();
        assert_eq!(
            signals[0].join_keys.account_id.as_deref(),
            Some("chalk-teacher")
        );
        assert_eq!(signals[0].affected_count, Some(7));

        let signals = GithubIssuesAdapter
            .normalize(&envelope("issues", serde_json::json!([minimal_issue()])))
            .unwrap();
        assert_eq!(signals[0].join_keys.account_id, None);
        assert_eq!(signals[0].affected_count, None);
    }

    #[test]
    fn malformed_issue_is_an_error_not_a_panic() {
        // Missing required `html_url`.
        let issue = serde_json::json!({
            "number": 118,
            "title": "Broken",
            "created_at": "2026-08-05T14:00:00Z",
            "updated_at": "2026-08-06T09:00:00Z"
        });
        assert!(matches!(
            GithubIssuesAdapter.normalize(&envelope("issues", serde_json::json!([issue]))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn missing_repo_context_is_an_error() {
        let input = serde_json::json!({
            "endpoint": "issues",
            "payload": [minimal_issue()],
        });
        assert!(matches!(
            GithubIssuesAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut issue = minimal_issue();
        issue["some_future_github_field"] = serde_json::json!({ "nested": 1 });
        let signals = GithubIssuesAdapter
            .normalize(&envelope("issues", serde_json::json!([issue])))
            .unwrap();
        assert_eq!(signals[0].raw["some_future_github_field"]["nested"], 1);
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("pulls", serde_json::json!([]));
        assert!(matches!(
            GithubIssuesAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
