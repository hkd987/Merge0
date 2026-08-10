//! Slack channel messages → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `messages` — a `conversations.history` API response for one designated
//!   channel (`{"messages": [...]}`), one `ticket` Signal per kept message.
//!
//! Envelope context:
//! `{"team_base_url": "https://acme-example.slack.com", "channel_id":
//! "C0123456789", "channel_name": "bugs"}` — the fetch layer records which
//! channel the page came from (Slack's response does not repeat it) and the
//! workspace base URL used to build archive permalinks.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Kind is `ticket`** by team convention: messages in designated channels
//!   (e.g. `#bugs`) are reports filed by humans, the moral equivalent of a
//!   help-desk ticket.
//! - **Skipped messages.** Messages carrying a `subtype` (joins, leaves,
//!   channel topic changes, bot posts) are channel noise, not reports — they
//!   are dropped. Messages carrying a `bot_id` are dropped too: the loop must
//!   never ingest its own notifications (feedback-loop guard) — a Merge0 bot
//!   posting triage updates into `#bugs` must not become a Signal again.
//! - **Timestamps.** Slack's `ts` is a string of epoch seconds with a
//!   fractional disambiguator (`"1723100000.000100"`); the part before the
//!   `.` is parsed as UNIX seconds for `first_seen` (sub-second precision is
//!   dropped). `last_seen` is `latest_reply` (same format) when the message
//!   has a thread, else `ts`.
//! - **Title/body.** The title is the first line of `text`, truncated to 120
//!   characters; the body is the full text.
//! - **Severity** is always medium: Slack carries no priority metadata, but
//!   these channels are an explicit watch list — a human deliberately posted
//!   to a designated bug channel, which is triage-by-convention. (Medium is
//!   exactly the gate's default floor: watched-channel messages reach the
//!   gate; the gate still decides.)
//! - **`affected_count`** is `reply_count + 1` when present, else `1` —
//!   thread participation as an impact proxy (the reporter plus repliers).
//! - **Evidence** is the Slack archive permalink,
//!   `{team_base_url}/archives/{channel_id}/p{ts-without-the-dot}` (Slack's
//!   permalink convention), kind `other` — Slack threads are neither tickets
//!   nor issues in the evidence taxonomy.
//! - **Fingerprint** hashes `{channel_id}:{thread_ts or ts}`, so every
//!   re-fetch of the same thread root dedupes to one Signal. The `user`
//!   field is deliberately not used for `join_keys` — a Slack user id
//!   identifies a teammate, not a customer account.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct SlackAdapter;

/// Typed context for the Slack envelope.
#[derive(Debug, Deserialize)]
struct Context {
    team_base_url: String,
    channel_id: String,
    channel_name: String,
}

#[derive(Debug, Deserialize)]
struct MessagesPage {
    messages: Vec<serde_json::Value>,
}

/// A Slack message — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Message {
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
    text: String,
    #[serde(default)]
    reply_count: Option<u64>,
    #[serde(default)]
    latest_reply: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
}

impl Adapter for SlackAdapter {
    fn source(&self) -> Source {
        Source::Slack
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid slack context: {e}")))?;

        match envelope.endpoint.as_str() {
            "messages" => {
                let page: MessagesPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"messages\": [...]}}: {e}"))
                    })?;
                page.messages
                    .iter()
                    .map(|message| normalize_message(message, &context))
                    .filter_map(Result::transpose)
                    .collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one message; `Ok(None)` means the message is skipped (see module
/// docs), never silently on malformed input.
fn normalize_message(
    raw: &serde_json::Value,
    context: &Context,
) -> Result<Option<Signal>, AdapterError> {
    let message: Message = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid slack message: {e}")))?;

    // Skip rules: channel-noise subtypes, and bot posts — the feedback-loop
    // guard (never re-ingest the loop's own notifications).
    if message.subtype.is_some() || message.bot_id.is_some() {
        return Ok(None);
    }

    let first_seen = parse_slack_ts(&message.ts)?;
    let last_seen = match &message.latest_reply {
        Some(reply_ts) => parse_slack_ts(reply_ts)?,
        None => first_seen,
    };

    let base_url = context.team_base_url.trim_end_matches('/');
    let channel_id = &context.channel_id;
    let permalink_ts = message.ts.replace('.', "");
    let thread_root = message.thread_ts.as_deref().unwrap_or(&message.ts);

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Slack,
        source_ref: format!("{channel_id}:{}", message.ts),
        kind: SignalKind::Ticket,
        severity: Severity::Medium,
        title: title_from_text(&message.text),
        body: message.text.clone(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: format!("Slack thread in #{}", context.channel_name),
            url: format!("{base_url}/archives/{channel_id}/p{permalink_ts}"),
        }],
        fingerprint: fingerprint(Source::Slack, &[&format!("{channel_id}:{thread_root}")]),
        join_keys: JoinKeys::default(),
        affected_count: Some(message.reply_count.map_or(1, |replies| replies + 1)),
        delegated: false,
        first_seen,
        last_seen,
        raw: raw.clone(),
    }))
}

/// Parse a Slack `ts` (`"1723100000.000100"`): the part before the `.` is
/// UNIX seconds; the fractional disambiguator is dropped (see module docs).
fn parse_slack_ts(ts: &str) -> Result<DateTime<Utc>, AdapterError> {
    let seconds = ts.split('.').next().unwrap_or(ts);
    let seconds: i64 = seconds
        .parse()
        .map_err(|_| AdapterError::Malformed(format!("invalid slack ts {ts:?}")))?;
    DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| AdapterError::Malformed(format!("slack ts {ts:?} out of range")))
}

/// First line of the message, truncated to 120 characters.
fn title_from_text(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default();
    first_line.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": {
                "team_base_url": "https://acme-example.slack.com",
                "channel_id": "C0123456789",
                "channel_name": "bugs",
            },
            "payload": payload,
        })
    }

    fn minimal_message() -> serde_json::Value {
        serde_json::json!({
            "ts": "1723100000.000100",
            "user": "U0EXAMPLE01",
            "text": "Roster export button does nothing on the districts page"
        })
    }

    fn normalize_one(message: serde_json::Value) -> Vec<Signal> {
        SlackAdapter
            .normalize(&envelope(
                "messages",
                serde_json::json!({ "messages": [message] }),
            ))
            .unwrap()
    }

    #[test]
    fn ts_parses_seconds_and_drops_fraction() {
        assert_eq!(
            parse_slack_ts("1723100000.000100").unwrap(),
            DateTime::from_timestamp(1_723_100_000, 0).unwrap()
        );
        // No fractional part is fine too.
        assert_eq!(
            parse_slack_ts("1723100000").unwrap(),
            DateTime::from_timestamp(1_723_100_000, 0).unwrap()
        );
    }

    #[test]
    fn malformed_ts_is_an_error_not_a_panic() {
        assert!(matches!(
            parse_slack_ts("not-a-ts"),
            Err(AdapterError::Malformed(_))
        ));
        let mut message = minimal_message();
        message["ts"] = serde_json::json!("yesterday.000100");
        assert!(matches!(
            SlackAdapter.normalize(&envelope(
                "messages",
                serde_json::json!({ "messages": [message] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn subtype_messages_are_skipped() {
        let mut message = minimal_message();
        message["subtype"] = serde_json::json!("channel_join");
        assert!(normalize_one(message).is_empty());
    }

    #[test]
    fn bot_messages_are_skipped_feedback_loop_guard() {
        let mut message = minimal_message();
        message["bot_id"] = serde_json::json!("B0EXAMPLE01");
        assert!(normalize_one(message).is_empty());
    }

    #[test]
    fn title_is_first_line_truncated_to_120_chars() {
        let mut message = minimal_message();
        let long_first_line = "x".repeat(200);
        message["text"] = serde_json::json!(format!("{long_first_line}\nsecond line"));
        let signals = normalize_one(message);
        assert_eq!(signals[0].title, "x".repeat(120));
        assert!(signals[0].body.contains("second line"));
    }

    #[test]
    fn reply_count_becomes_participant_affected_count() {
        let mut message = minimal_message();
        message["reply_count"] = serde_json::json!(4);
        assert_eq!(normalize_one(message)[0].affected_count, Some(5));
        assert_eq!(normalize_one(minimal_message())[0].affected_count, Some(1));
    }

    #[test]
    fn last_seen_prefers_latest_reply() {
        let mut message = minimal_message();
        message["latest_reply"] = serde_json::json!("1723186400.000200");
        let signal = &normalize_one(message)[0];
        assert_eq!(
            signal.first_seen,
            DateTime::from_timestamp(1_723_100_000, 0).unwrap()
        );
        assert_eq!(
            signal.last_seen,
            DateTime::from_timestamp(1_723_186_400, 0).unwrap()
        );
        let signal = &normalize_one(minimal_message())[0];
        assert_eq!(signal.first_seen, signal.last_seen);
    }

    #[test]
    fn fingerprint_uses_thread_root_so_replies_dedupe() {
        let root = &normalize_one(minimal_message())[0];
        // A later re-fetch where the same message anchors a thread.
        let mut threaded = minimal_message();
        threaded["thread_ts"] = serde_json::json!("1723100000.000100");
        threaded["reply_count"] = serde_json::json!(2);
        assert_eq!(normalize_one(threaded)[0].fingerprint, root.fingerprint);

        let mut other = minimal_message();
        other["ts"] = serde_json::json!("1723190000.000300");
        assert_ne!(normalize_one(other)[0].fingerprint, root.fingerprint);
    }

    #[test]
    fn evidence_permalink_removes_the_dot() {
        let signals = normalize_one(minimal_message());
        assert_eq!(
            signals[0].evidence[0].url,
            "https://acme-example.slack.com/archives/C0123456789/p1723100000000100"
        );
        assert_eq!(signals[0].evidence[0].label, "Slack thread in #bugs");
    }

    #[test]
    fn malformed_message_is_an_error_not_a_panic() {
        // Missing required `text`.
        let message = serde_json::json!({ "ts": "1723100000.000100" });
        assert!(matches!(
            SlackAdapter.normalize(&envelope(
                "messages",
                serde_json::json!({ "messages": [message] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("channels", serde_json::json!({ "messages": [] }));
        match SlackAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "channels"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }
}
