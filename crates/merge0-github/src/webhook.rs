//! Webhook intake: signature verification and event parsing.
//!
//! Revert detection (PRD P0-8): a push whose commit message contains
//! `This reverts commit <sha>` — or a merged PR titled `Revert "..."` whose
//! body carries the same marker — maps back to the original Merge0 PR via
//! its recorded merge SHA and becomes a `Reverted` hard negative when it
//! lands within 14 days of the merge.

use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The revert-as-hard-negative window (PRD P0-8).
pub const REVERT_WINDOW_DAYS: i64 = 14;

/// Verify `X-Hub-Signature-256: sha256=<hex>` (constant-time).
pub fn verify_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_signature) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_signature) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[derive(Debug, Clone, PartialEq)]
pub enum WebhookEvent {
    PrMerged {
        pr_url: String,
        merged_at: DateTime<Utc>,
        merge_sha: Option<String>,
        title: String,
        body: String,
    },
    PrClosed {
        pr_url: String,
        closed_at: DateTime<Utc>,
    },
    Release {
        tag: String,
        published_at: DateTime<Utc>,
        sha: Option<String>,
        notes: Option<String>,
    },
    Push {
        commits: Vec<PushCommit>,
    },
    /// Anything we don't consume — ignored, never an error.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PushCommit {
    pub sha: String,
    pub message: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("malformed {event} payload: {reason}")]
    Malformed { event: String, reason: String },
}

/// Parse a GitHub webhook by `X-GitHub-Event` name.
pub fn parse(event_name: &str, payload: &serde_json::Value) -> Result<WebhookEvent, WebhookError> {
    let malformed = |reason: &str| WebhookError::Malformed {
        event: event_name.to_string(),
        reason: reason.to_string(),
    };
    match event_name {
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or_default();
            if action != "closed" {
                return Ok(WebhookEvent::Other);
            }
            let pr = &payload["pull_request"];
            let pr_url = pr["html_url"]
                .as_str()
                .ok_or_else(|| malformed("no pull_request.html_url"))?
                .to_string();
            if pr["merged"].as_bool() == Some(true) {
                Ok(WebhookEvent::PrMerged {
                    pr_url,
                    merged_at: pr["merged_at"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| malformed("no merged_at"))?,
                    merge_sha: pr["merge_commit_sha"].as_str().map(String::from),
                    title: pr["title"].as_str().unwrap_or_default().to_string(),
                    body: pr["body"].as_str().unwrap_or_default().to_string(),
                })
            } else {
                Ok(WebhookEvent::PrClosed {
                    pr_url,
                    closed_at: pr["closed_at"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(Utc::now),
                })
            }
        }
        "release" => {
            if payload["action"].as_str() != Some("published") {
                return Ok(WebhookEvent::Other);
            }
            let release = &payload["release"];
            Ok(WebhookEvent::Release {
                tag: release["tag_name"]
                    .as_str()
                    .ok_or_else(|| malformed("no tag_name"))?
                    .to_string(),
                published_at: release["published_at"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| malformed("no published_at"))?,
                sha: payload["release"]["target_commitish"]
                    .as_str()
                    .map(String::from),
                notes: release["body"].as_str().map(String::from),
            })
        }
        "push" => {
            let commits = payload["commits"]
                .as_array()
                .map(|commits| {
                    commits
                        .iter()
                        .filter_map(|c| {
                            Some(PushCommit {
                                sha: c["id"].as_str()?.to_string(),
                                message: c["message"].as_str().unwrap_or_default().to_string(),
                                timestamp: c["timestamp"].as_str().and_then(|s| s.parse().ok()),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(WebhookEvent::Push { commits })
        }
        _ => Ok(WebhookEvent::Other),
    }
}

/// Extract the SHA a commit message reverts, if any.
pub fn reverted_sha(message: &str) -> Option<String> {
    let marker = "This reverts commit ";
    let start = message.find(marker)? + marker.len();
    let sha: String = message[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (sha.len() >= 7).then_some(sha)
}

/// Is a revert at `reverted_at` within the hard-negative window of the
/// original merge?
pub fn within_revert_window(merged_at: DateTime<Utc>, reverted_at: DateTime<Utc>) -> bool {
    reverted_at >= merged_at && reverted_at - merged_at <= Duration::days(REVERT_WINDOW_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_verify_accepts_valid_rejects_forged() {
        let body = br#"{"action":"closed"}"#;
        let header = sign("hook-secret", body);
        assert!(verify_signature("hook-secret", body, &header));
        assert!(!verify_signature("wrong-secret", body, &header));
        assert!(!verify_signature("hook-secret", b"tampered", &header));
        assert!(!verify_signature("hook-secret", body, "sha256=nothex"));
        assert!(!verify_signature("hook-secret", body, "sha1=deadbeef"));
    }

    #[test]
    fn merged_pr_parses_with_merge_sha() {
        let payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "html_url": "https://github.com/chalk/chalk/pull/9",
                "merged": true,
                "merged_at": "2026-08-06T10:00:00Z",
                "merge_commit_sha": "abc1234def",
                "title": "Fix crash",
                "body": "fixes it",
            }
        });
        match parse("pull_request", &payload).unwrap() {
            WebhookEvent::PrMerged {
                pr_url, merge_sha, ..
            } => {
                assert_eq!(pr_url, "https://github.com/chalk/chalk/pull/9");
                assert_eq!(merge_sha.as_deref(), Some("abc1234def"));
            }
            other => panic!("expected merged, got {other:?}"),
        }
    }

    #[test]
    fn closed_unmerged_pr_is_closed_and_open_action_ignored() {
        let payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "html_url": "https://github.com/chalk/chalk/pull/9",
                "merged": false,
                "closed_at": "2026-08-06T10:00:00Z",
            }
        });
        assert!(matches!(
            parse("pull_request", &payload).unwrap(),
            WebhookEvent::PrClosed { .. }
        ));
        let opened = serde_json::json!({"action": "opened", "pull_request": {}});
        assert_eq!(parse("pull_request", &opened).unwrap(), WebhookEvent::Other);
    }

    #[test]
    fn release_and_push_parse_and_unknown_events_are_other() {
        let release = serde_json::json!({
            "action": "published",
            "release": {
                "tag_name": "v2.4.0",
                "published_at": "2026-08-01T00:00:00Z",
                "target_commitish": "beefcafe",
                "body": "notes",
            }
        });
        assert!(matches!(
            parse("release", &release).unwrap(),
            WebhookEvent::Release { tag, .. } if tag == "v2.4.0"
        ));

        let push = serde_json::json!({
            "commits": [
                {"id": "aaa", "message": "normal commit", "timestamp": "2026-08-07T00:00:00Z"},
                {"id": "bbb", "message": "Revert \"Fix crash\"\n\nThis reverts commit abc1234def.", "timestamp": "2026-08-07T00:00:00Z"},
            ]
        });
        match parse("push", &push).unwrap() {
            WebhookEvent::Push { commits } => assert_eq!(commits.len(), 2),
            other => panic!("expected push, got {other:?}"),
        }
        assert_eq!(
            parse("star", &serde_json::json!({})).unwrap(),
            WebhookEvent::Other
        );
    }

    #[test]
    fn revert_sha_extraction() {
        assert_eq!(
            reverted_sha("Revert \"x\"\n\nThis reverts commit abc1234def5678."),
            Some("abc1234def5678".into())
        );
        assert_eq!(reverted_sha("This reverts commit abc."), None, "too short");
        assert_eq!(reverted_sha("ordinary message"), None);
    }

    #[test]
    fn revert_window_is_14_days_inclusive() {
        let merged = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        assert!(within_revert_window(merged, merged + Duration::days(14)));
        assert!(!within_revert_window(merged, merged + Duration::days(15)));
        assert!(!within_revert_window(merged, merged - Duration::hours(1)));
    }
}
