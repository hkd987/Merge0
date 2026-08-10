//! Intercom → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `conversations` — the Intercom list-conversations API response
//!   (`{"conversations": [...]}`), one `ticket` Signal per open conversation.
//!
//! Envelope context:
//! `{"app_base_url": "https://app.intercom-example.com/a/inbox/abc123"}` —
//! used to build inbox deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Skipped conversations.** `state == "closed"` conversations are
//!   resolved history, not live demand — they are dropped rather than
//!   re-surfaced on every fetch.
//! - **Severity** maps from the Intercom `priority`: `"priority"` → high,
//!   anything else or absent → low. This mirrors the Zendesk absent→low
//!   convention: a conversation nobody flagged has not demonstrated urgency.
//! - **Title** is the conversation `title`; Intercom leaves it null for most
//!   in-app conversations, so a null/absent title falls back to the first
//!   120 characters of the body text.
//! - **Body** is `source.body` with HTML tags stripped by a simple char-walk
//!   state machine (`<br>` and closing `</p>` become newlines). HTML entity
//!   decoding is out of scope — `&amp;` and friends pass through verbatim.
//! - **`first_seen`/`last_seen`** come from `created_at`/`updated_at`, which
//!   Intercom sends as UNIX integer seconds (not RFC 3339) — converted here.
//! - **`affected_count`** is `statistics.count_conversation_parts` when
//!   present — back-and-forth volume as an impact proxy.
//! - **`join_keys.account_id`** is `source.author.id` only when
//!   `author.type == "user"` — an end-user reporter identifies the affected
//!   account; admins, bots, and leads do not.
//! - **Fingerprint** hashes the conversation `id`, Intercom's stable
//!   identifier across re-fetches.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct IntercomAdapter;

/// Typed context for the Intercom envelope.
#[derive(Debug, Deserialize)]
struct Context {
    app_base_url: String,
}

#[derive(Debug, Deserialize)]
struct ConversationsPage {
    conversations: Vec<serde_json::Value>,
}

/// An Intercom conversation — only the fields we normalize; everything else
/// is preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Conversation {
    id: String,
    #[serde(default)]
    title: Option<String>,
    source: ConversationSource,
    created_at: i64,
    updated_at: i64,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    statistics: Option<Statistics>,
}

/// The initiating message of a conversation.
#[derive(Debug, Deserialize)]
struct ConversationSource {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    author: Option<Author>,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(rename = "type")]
    kind: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct Statistics {
    #[serde(default)]
    count_conversation_parts: Option<u64>,
}

impl Adapter for IntercomAdapter {
    fn source(&self) -> Source {
        Source::Intercom
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid intercom context: {e}")))?;
        let base_url = context.app_base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "conversations" => {
                let page: ConversationsPage = serde_json::from_value(envelope.payload.clone())
                    .map_err(|e| {
                        AdapterError::Malformed(format!(
                            "expected {{\"conversations\": [...]}}: {e}"
                        ))
                    })?;
                page.conversations
                    .iter()
                    .map(|conversation| normalize_conversation(conversation, &base_url))
                    .filter_map(Result::transpose)
                    .collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one conversation; `Ok(None)` means it is skipped (closed — see
/// module docs), never silently on malformed input.
fn normalize_conversation(
    raw: &serde_json::Value,
    base_url: &str,
) -> Result<Option<Signal>, AdapterError> {
    let conversation: Conversation = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid intercom conversation: {e}")))?;

    // Skip rule: closed conversations are resolved history, not live demand.
    if conversation.state.as_deref() == Some("closed") {
        return Ok(None);
    }

    let id = conversation.id.clone();
    let body = strip_html(conversation.source.body.as_deref().unwrap_or_default());
    let title = match conversation.title {
        Some(title) => title,
        None => body.chars().take(120).collect(),
    };

    let account_id = conversation
        .source
        .author
        .as_ref()
        .filter(|author| author.kind == "user")
        .map(|author| author.id.clone());

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Intercom,
        source_ref: id.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_priority(conversation.priority.as_deref()),
        title,
        body,
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Intercom conversation {id}"),
            url: format!("{base_url}/conversation/{id}"),
        }],
        fingerprint: fingerprint(Source::Intercom, &[&id]),
        join_keys: JoinKeys {
            account_id,
            ..Default::default()
        },
        affected_count: conversation
            .statistics
            .and_then(|stats| stats.count_conversation_parts),
        delegated: false,
        first_seen: parse_unix_seconds(conversation.created_at, "created_at")?,
        last_seen: parse_unix_seconds(conversation.updated_at, "updated_at")?,
        raw: raw.clone(),
    }))
}

/// Intercom timestamps are UNIX integer seconds (see module docs).
fn parse_unix_seconds(seconds: i64, field: &str) -> Result<DateTime<Utc>, AdapterError> {
    DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| AdapterError::Malformed(format!("{field} {seconds} out of range")))
}

/// Intercom `priority` → severity (see module docs).
fn severity_from_priority(priority: Option<&str>) -> Severity {
    match priority {
        Some("priority") => Severity::High,
        _ => Severity::Low,
    }
}

/// Strip HTML tags with a char-walk state machine: text outside `<...>` is
/// kept, `<br>` and closing `</p>` become newlines, everything else between
/// `<` and `>` is dropped. Entity decoding is out of scope (see module docs).
fn strip_html(html: &str) -> String {
    let mut text = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .trim_end_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let breaks = name == "br" || (name == "p" && tag.starts_with('/'));
                if breaks && !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            text.push(ch);
        }
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": {
                "app_base_url": "https://app.intercom-example.com/a/inbox/abc123"
            },
            "payload": payload,
        })
    }

    fn minimal_conversation() -> serde_json::Value {
        serde_json::json!({
            "id": "70012345",
            "source": { "body": "<p>The reports page never finishes loading.</p>" },
            "created_at": 1723100000,
            "updated_at": 1723186400
        })
    }

    fn normalize_one(conversation: serde_json::Value) -> Vec<Signal> {
        IntercomAdapter
            .normalize(&envelope(
                "conversations",
                serde_json::json!({ "conversations": [conversation] }),
            ))
            .unwrap()
    }

    #[test]
    fn unix_seconds_are_converted() {
        let signal = &normalize_one(minimal_conversation())[0];
        assert_eq!(
            signal.first_seen,
            DateTime::from_timestamp(1_723_100_000, 0).unwrap()
        );
        assert_eq!(
            signal.last_seen,
            DateTime::from_timestamp(1_723_186_400, 0).unwrap()
        );
    }

    #[test]
    fn out_of_range_timestamp_is_an_error_not_a_panic() {
        let mut conversation = minimal_conversation();
        conversation["created_at"] = serde_json::json!(i64::MAX);
        assert!(matches!(
            IntercomAdapter.normalize(&envelope(
                "conversations",
                serde_json::json!({ "conversations": [conversation] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn html_stripping_drops_tags_and_breaks_paragraphs() {
        assert_eq!(
            strip_html("<p>First paragraph</p><p>Second with <b>bold</b> text</p>"),
            "First paragraph\nSecond with bold text"
        );
        assert_eq!(strip_html("line one<br/>line two"), "line one\nline two");
        assert_eq!(
            strip_html("<a href=\"https://example.com\">a link</a>"),
            "a link"
        );
        // Entity decoding is out of scope.
        assert_eq!(strip_html("<p>salt &amp; pepper</p>"), "salt &amp; pepper");
        assert_eq!(strip_html("plain text"), "plain text");
    }

    #[test]
    fn closed_conversations_are_skipped() {
        let mut conversation = minimal_conversation();
        conversation["state"] = serde_json::json!("closed");
        assert!(normalize_one(conversation).is_empty());

        let mut conversation = minimal_conversation();
        conversation["state"] = serde_json::json!("open");
        assert_eq!(normalize_one(conversation).len(), 1);
    }

    #[test]
    fn priority_mapping() {
        assert_eq!(severity_from_priority(Some("priority")), Severity::High);
        assert_eq!(severity_from_priority(Some("not_priority")), Severity::Low);
        assert_eq!(severity_from_priority(Some("unheard-of")), Severity::Low);
        assert_eq!(severity_from_priority(None), Severity::Low);
    }

    #[test]
    fn null_title_falls_back_to_body_text() {
        let signal = &normalize_one(minimal_conversation())[0];
        assert_eq!(signal.title, "The reports page never finishes loading.");

        let mut conversation = minimal_conversation();
        conversation["title"] = serde_json::json!("Reports page hangs");
        assert_eq!(normalize_one(conversation)[0].title, "Reports page hangs");

        // Fallback truncates to 120 chars of the stripped body.
        let mut conversation = minimal_conversation();
        conversation["source"]["body"] = serde_json::json!(format!("<p>{}</p>", "y".repeat(300)));
        assert_eq!(normalize_one(conversation)[0].title, "y".repeat(120));
    }

    #[test]
    fn account_id_only_from_end_user_authors() {
        let mut conversation = minimal_conversation();
        conversation["source"]["author"] =
            serde_json::json!({ "type": "user", "id": "6401ab234cde567890f12a34" });
        assert_eq!(
            normalize_one(conversation)[0]
                .join_keys
                .account_id
                .as_deref(),
            Some("6401ab234cde567890f12a34")
        );

        let mut conversation = minimal_conversation();
        conversation["source"]["author"] = serde_json::json!({ "type": "admin", "id": "991234" });
        assert_eq!(normalize_one(conversation)[0].join_keys.account_id, None);

        assert_eq!(
            normalize_one(minimal_conversation())[0]
                .join_keys
                .account_id,
            None
        );
    }

    #[test]
    fn conversation_parts_become_affected_count() {
        let mut conversation = minimal_conversation();
        conversation["statistics"] = serde_json::json!({ "count_conversation_parts": 7 });
        assert_eq!(normalize_one(conversation)[0].affected_count, Some(7));
        assert_eq!(
            normalize_one(minimal_conversation())[0].affected_count,
            None
        );
    }

    #[test]
    fn evidence_deep_link_uses_app_base_url() {
        let signal = &normalize_one(minimal_conversation())[0];
        assert_eq!(
            signal.evidence[0].url,
            "https://app.intercom-example.com/a/inbox/abc123/conversation/70012345"
        );
        assert_eq!(signal.evidence[0].label, "Intercom conversation 70012345");
    }

    #[test]
    fn malformed_conversation_is_an_error_not_a_panic() {
        // Missing required `source`.
        let conversation = serde_json::json!({
            "id": "70012345",
            "created_at": 1723100000,
            "updated_at": 1723186400
        });
        assert!(matches!(
            IntercomAdapter.normalize(&envelope(
                "conversations",
                serde_json::json!({ "conversations": [conversation] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("contacts", serde_json::json!({ "conversations": [] }));
        match IntercomAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "contacts"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }
}
