//! Native webhook support (PRD §1: adapters "poll or receive webhooks").
//!
//! Pure helpers the server mounts — no axum here:
//!
//! - signature verification per vendor scheme (HMAC hex for Sentry, HMAC
//!   base64-of-`timestamp+body` for Zendesk, a shared token for
//!   PostHog/Datadog whose webhooks have no vendor signature scheme);
//! - envelope builders converting a native webhook payload into the same
//!   `{endpoint, context, payload}` envelope the pollers produce, so one
//!   adapter serves both delivery paths. Unrecognized shapes return `None`
//!   (the server answers 400).
//!
//! All comparisons of secrets/MACs are constant-time.

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Constant-time byte equality (for equal lengths; length itself is not
/// secret). Never short-circuits on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hmac_sha256(key: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("hmac accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().to_vec()
}

/// Sentry `sentry-hook-signature`: HMAC-SHA256 of the raw body with the
/// integration's client secret, hex-encoded.
pub fn verify_sentry_signature(client_secret: &str, body: &[u8], signature_hex: &str) -> bool {
    let expected = hex::encode(hmac_sha256(client_secret, &[body]));
    constant_time_eq(
        expected.as_bytes(),
        signature_hex.trim().to_ascii_lowercase().as_bytes(),
    )
}

/// Zendesk `x-zendesk-webhook-signature`: HMAC-SHA256 of
/// `{timestamp}{body}` with the webhook signing secret, base64-encoded
/// (timestamp from `x-zendesk-webhook-signature-timestamp`).
pub fn verify_zendesk_signature(
    signing_secret: &str,
    timestamp: &str,
    body: &[u8],
    signature_b64: &str,
) -> bool {
    let expected = base64_encode(&hmac_sha256(signing_secret, &[timestamp.as_bytes(), body]));
    constant_time_eq(expected.as_bytes(), signature_b64.trim().as_bytes())
}

/// Shared-token check for vendors without a signature scheme (PostHog and
/// Datadog webhooks carry a caller-configured token).
pub fn verify_shared_token(expected: &str, presented: &str) -> bool {
    constant_time_eq(expected.as_bytes(), presented.as_bytes())
}

// ---- envelope builders ----

/// Sentry issue webhook (`{"action": ..., "data": {"issue": {...}}}`) →
/// the `issues` envelope with a single-element array, exactly what the
/// Sentry adapter's poll path consumes.
pub fn sentry_webhook_to_envelope(payload: &Value) -> Option<Value> {
    let issue = payload.get("data")?.get("issue")?;
    if !issue.is_object() {
        return None;
    }
    Some(json!({
        "endpoint": "issues",
        "context": {},
        "payload": [issue],
    }))
}

/// PostHog webhook → envelope. Recognized shapes:
///
/// - a CDP webhook-destination delivery `{"event": {...}}` (or the event
///   object itself) whose `event` name is `$rageclick` → `rageclick_events`;
/// - an error-tracking issue object (`id`/`name`/`first_seen`/`last_seen`),
///   optionally under an `"issue"` key → `error_tracking_issues`.
pub fn posthog_webhook_to_envelope(payload: &Value, project_base_url: &str) -> Option<Value> {
    let context = json!({ "project_base_url": project_base_url });

    // Rage-click event, wrapped or bare.
    let event = match payload.get("event") {
        Some(Value::Object(_)) => payload.get("event"),
        Some(Value::String(_)) => Some(payload),
        _ => None,
    };
    if let Some(event) = event {
        if event.get("event").and_then(Value::as_str) == Some("$rageclick") {
            return Some(json!({
                "endpoint": "rageclick_events",
                "context": context,
                "payload": { "results": [event] },
            }));
        }
        return None;
    }

    // Error-tracking issue, wrapped or bare.
    let issue = payload.get("issue").unwrap_or(payload);
    let looks_like_issue = issue.is_object()
        && ["id", "name", "first_seen", "last_seen"]
            .iter()
            .all(|key| issue.get(key).is_some());
    if looks_like_issue {
        return Some(json!({
            "endpoint": "error_tracking_issues",
            "context": context,
            "payload": { "results": [issue] },
        }));
    }
    None
}

/// Zendesk webhook → the `tickets` envelope. Recognized shapes: the
/// event-style delivery `{"type": "zen:...", "detail": {...ticket}}` or a
/// trigger-built `{"ticket": {...}}`.
pub fn zendesk_webhook_to_envelope(payload: &Value, agent_base_url: &str) -> Option<Value> {
    let ticket = payload.get("detail").or_else(|| payload.get("ticket"))?;
    if !ticket.is_object() || ticket.get("id").is_none() {
        return None;
    }
    Some(json!({
        "endpoint": "tickets",
        "context": { "agent_base_url": agent_base_url },
        "payload": { "tickets": [ticket] },
    }))
}

/// Datadog webhook-integration payload (the documented `$ID`/`$EVENT_TITLE`/
/// `$DATE`/... template variables) → the Events-API-v2-shaped `events`
/// envelope the Datadog adapter consumes. Requires `id`, a title
/// (`event_title` or `title`) and `date` (epoch milliseconds).
pub fn datadog_webhook_to_envelope(payload: &Value, app_base_url: &str) -> Option<Value> {
    let id = string_or_number(payload.get("id")?)?;
    let title = payload
        .get("event_title")
        .or_else(|| payload.get("title"))?
        .as_str()?
        .to_string();
    let millis = match payload.get("date")? {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    let timestamp = chrono::DateTime::from_timestamp_millis(millis)?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let mut attributes = json!({ "timestamp": timestamp, "title": title });
    if let Some(message) = payload
        .get("event_msg")
        .or_else(|| payload.get("body"))
        .and_then(Value::as_str)
    {
        attributes["message"] = json!(message);
    }
    if let Some(alert_type) = payload.get("alert_type").and_then(Value::as_str) {
        attributes["alert_type"] = json!(alert_type);
    }
    if let Some(monitor_id) = payload
        .get("alert_id")
        .and_then(|v| string_or_number(v)?.parse::<u64>().ok())
    {
        attributes["monitor_id"] = json!(monitor_id);
    }
    // `$TAGS` renders comma-separated; an array is accepted too.
    match payload.get("tags") {
        Some(Value::String(tags)) => {
            let tags: Vec<&str> = tags.split(',').map(str::trim).collect();
            attributes["tags"] = json!(tags);
        }
        Some(Value::Array(tags)) => attributes["tags"] = json!(tags),
        _ => {}
    }

    Some(json!({
        "endpoint": "events",
        "context": { "app_base_url": app_base_url },
        "payload": { "data": [{ "id": id, "type": "event", "attributes": attributes }] },
    }))
}

/// Linear `linear-signature`: HMAC-SHA256 of the raw body with the webhook
/// signing secret, hex-encoded.
pub fn verify_linear_signature(signing_secret: &str, body: &[u8], signature_hex: &str) -> bool {
    let expected = hex::encode(hmac_sha256(signing_secret, &[body]));
    constant_time_eq(
        expected.as_bytes(),
        signature_hex.trim().to_ascii_lowercase().as_bytes(),
    )
}

/// Slack Events API request signing: `v0=` + HMAC-SHA256 of
/// `v0:{timestamp}:{body}` with the app's signing secret. (Same scheme the
/// interactions endpoint verifies via merge0-slack; duplicated here so the
/// fetch layer stays free of the notification crate.)
pub fn verify_slack_events_signature(
    signing_secret: &str,
    timestamp: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    let mac = hmac_sha256(signing_secret, &[b"v0:", timestamp.as_bytes(), b":", body]);
    let expected = format!("v0={}", hex::encode(mac));
    constant_time_eq(expected.as_bytes(), signature.trim().as_bytes())
}

/// Jira webhook → adapter envelope. Jira Automation/webhooks POST
/// `{"webhookEvent": "jira:issue_created|updated", "issue": {...}}`; the
/// issue slots into the adapter's `issues` page shape. Deleted-issue events
/// are ignored (nothing to normalize).
pub fn jira_webhook_to_envelope(payload: &Value, browse_base_url: &str) -> Option<Value> {
    if payload
        .get("webhookEvent")
        .and_then(Value::as_str)
        .is_some_and(|event| event.ends_with("deleted"))
    {
        return None;
    }
    let issue = payload.get("issue")?;
    issue.get("key")?;
    Some(json!({
        "endpoint": "issues",
        "context": { "browse_base_url": browse_base_url },
        "payload": { "issues": [issue] },
    }))
}

/// Linear webhook → adapter envelope. Linear POSTs
/// `{"type": "Issue", "action": "create|update|remove", "data": {...}}`.
/// Only Issue create/update carry a normalizable node; `remove` and other
/// entity types are ignored.
pub fn linear_webhook_to_envelope(payload: &Value) -> Option<Value> {
    if payload.get("type").and_then(Value::as_str) != Some("Issue") {
        return None;
    }
    if payload.get("action").and_then(Value::as_str) == Some("remove") {
        return None;
    }
    let node = payload.get("data")?;
    node.get("identifier")?;
    Some(json!({
        "endpoint": "issues",
        "context": {},
        "payload": { "nodes": [node] },
    }))
}

/// Slack Events API → adapter envelope. An `event_callback` whose inner
/// event is a channel `message` becomes a one-message `messages` page; the
/// channel id doubles as the display name when no mapping is configured
/// (the poller path carries real names). URL-verification handshakes and
/// non-message events return None — the caller answers those itself.
pub fn slack_event_to_envelope(payload: &Value, team_base_url: &str) -> Option<Value> {
    if payload.get("type").and_then(Value::as_str) != Some("event_callback") {
        return None;
    }
    let event = payload.get("event")?;
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?;
    Some(json!({
        "endpoint": "messages",
        "context": {
            "team_base_url": team_base_url,
            "channel_id": channel,
            "channel_name": channel,
        },
        "payload": { "messages": [event] },
    }))
}

fn string_or_number(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Minimal standard-alphabet padded base64 encoder — same contract as
/// `merge0-github`'s private `base64_mini::encode`; copied rather than
/// depending on a base64 crate for this single use.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_adapters::Adapter;
    use merge0_signal::{SignalKind, Source};

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    // ---- signatures ----

    #[test]
    fn linear_signature_accepts_valid_and_rejects_tampering() {
        let secret = "example-linear-signing";
        let body = br#"{"type":"Issue","action":"update"}"#;
        let valid = hex::encode(hmac_sha256(secret, &[body]));
        assert!(verify_linear_signature(secret, body, &valid));
        assert!(verify_linear_signature(secret, body, &valid.to_uppercase()));
        assert!(!verify_linear_signature(secret, body, "deadbeef"));
        assert!(!verify_linear_signature("other-secret", body, &valid));
    }

    #[test]
    fn slack_events_signature_matches_the_v0_scheme() {
        let secret = "example-slack-signing";
        let timestamp = "1723100000";
        let body = br#"{"type":"event_callback"}"#;
        let mac = hmac_sha256(
            secret,
            &[b"v0:", timestamp.as_bytes(), b":", body.as_slice()],
        );
        let valid = format!("v0={}", hex::encode(mac));
        assert!(verify_slack_events_signature(
            secret, timestamp, body, &valid
        ));
        assert!(!verify_slack_events_signature(
            secret,
            "1723100001",
            body,
            &valid
        ));
        assert!(!verify_slack_events_signature(
            secret,
            timestamp,
            body,
            "v0=deadbeef"
        ));
    }

    // ---- ticket-source webhook envelopes ----

    #[test]
    fn jira_webhook_wraps_the_issue_and_ignores_deletions() {
        let payload = serde_json::json!({
            "webhookEvent": "jira:issue_updated",
            "issue": {
                "key": "CHK-42",
                "fields": {
                    "summary": "Roster import stalls",
                    "priority": { "name": "High" },
                    "status": { "statusCategory": { "key": "indeterminate" } },
                    "created": "2026-08-05T10:00:00.000+0000",
                    "updated": "2026-08-06T11:00:00.000+0000"
                }
            }
        });
        let envelope =
            jira_webhook_to_envelope(&payload, "https://acme-example.atlassian.net/browse")
                .expect("issue event maps");
        let signals = merge0_adapter_jira::JiraAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Jira);
        assert_eq!(signals[0].kind, SignalKind::Ticket);

        let deleted = serde_json::json!({
            "webhookEvent": "jira:issue_deleted",
            "issue": { "key": "CHK-42" }
        });
        assert!(jira_webhook_to_envelope(&deleted, "https://x.example.com").is_none());
        assert!(
            jira_webhook_to_envelope(&serde_json::json!({}), "https://x.example.com").is_none()
        );
    }

    #[test]
    fn linear_webhook_wraps_issue_nodes_and_ignores_removals_and_other_types() {
        let payload = serde_json::json!({
            "type": "Issue",
            "action": "update",
            "data": {
                "identifier": "ENG-123",
                "title": "Export empty for large classes",
                "priority": 2,
                "createdAt": "2026-08-05T10:00:00.000Z",
                "updatedAt": "2026-08-06T11:00:00.000Z",
                "url": "https://linear.example.com/acme/issue/ENG-123"
            }
        });
        let envelope = linear_webhook_to_envelope(&payload).expect("issue update maps");
        let signals = merge0_adapter_linear::LinearAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Linear);

        let removal = serde_json::json!({"type": "Issue", "action": "remove", "data": {"identifier": "ENG-1"}});
        assert!(linear_webhook_to_envelope(&removal).is_none());
        let comment = serde_json::json!({"type": "Comment", "action": "create", "data": {}});
        assert!(linear_webhook_to_envelope(&comment).is_none());
    }

    #[test]
    fn slack_event_wraps_channel_messages_and_ignores_handshakes() {
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "channel": "C0123456789",
                "ts": "1723100000.000100",
                "text": "Gradebook import is failing for big classes",
                "user": "U0456"
            }
        });
        let envelope = slack_event_to_envelope(&payload, "https://acme-example.slack.com")
            .expect("message event maps");
        let signals = merge0_adapter_slack::SlackAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Slack);

        let handshake = serde_json::json!({"type": "url_verification", "challenge": "abc"});
        assert!(slack_event_to_envelope(&handshake, "https://x.example.com").is_none());
        let reaction = serde_json::json!({
            "type": "event_callback",
            "event": {"type": "reaction_added", "channel": "C1"}
        });
        assert!(slack_event_to_envelope(&reaction, "https://x.example.com").is_none());
    }

    #[test]
    fn sentry_signature_accepts_valid_and_rejects_tampering() {
        let secret = "example-client-secret";
        let body = br#"{"action":"created"}"#;
        let valid = hex::encode(hmac_sha256(secret, &[body]));

        assert!(verify_sentry_signature(secret, body, &valid));
        // Case-insensitive hex, tolerant of surrounding whitespace.
        assert!(verify_sentry_signature(secret, body, &valid.to_uppercase()));
        assert!(verify_sentry_signature(secret, body, &format!(" {valid} ")));
        // Tampered body, wrong secret, truncated/garbage signature.
        assert!(!verify_sentry_signature(
            secret,
            br#"{"action":"x"}"#,
            &valid
        ));
        assert!(!verify_sentry_signature("other-secret", body, &valid));
        assert!(!verify_sentry_signature(
            secret,
            body,
            &valid[..valid.len() - 2]
        ));
        assert!(!verify_sentry_signature(secret, body, "not-hex-at-all"));
        assert!(!verify_sentry_signature(secret, body, ""));
    }

    #[test]
    fn zendesk_signature_binds_timestamp_and_body() {
        let secret = "example-signing-secret";
        let timestamp = "2026-08-07T00:00:00Z";
        let body = br#"{"detail":{"id":1}}"#;
        let valid = base64_encode(&hmac_sha256(secret, &[timestamp.as_bytes(), body]));

        assert!(verify_zendesk_signature(secret, timestamp, body, &valid));
        // Replayed with a different timestamp → the MAC no longer matches.
        assert!(!verify_zendesk_signature(
            secret,
            "2026-08-08T00:00:00Z",
            body,
            &valid
        ));
        assert!(!verify_zendesk_signature(
            secret,
            timestamp,
            br#"{"detail":{"id":2}}"#,
            &valid
        ));
        assert!(!verify_zendesk_signature("other", timestamp, body, &valid));
        assert!(!verify_zendesk_signature(secret, timestamp, body, "AAAA"));
    }

    #[test]
    fn shared_token_exact_match_only() {
        assert!(verify_shared_token("tok-123", "tok-123"));
        assert!(!verify_shared_token("tok-123", "tok-124"));
        assert!(!verify_shared_token("tok-123", "tok-12"));
        assert!(!verify_shared_token("tok-123", ""));
    }

    // ---- envelope builders: each round-trips through the real adapter ----

    #[test]
    fn sentry_webhook_round_trips_through_adapter() {
        let webhook = json!({
            "action": "created",
            "data": { "issue": {
                "id": "5312345678",
                "title": "TypeError: x is undefined",
                "permalink": "https://sentry.example.com/organizations/acme/issues/5312345678/",
                "level": "error",
                "metadata": { "type": "TypeError" },
                "firstSeen": "2026-08-01T04:12:00Z",
                "lastSeen": "2026-08-06T22:30:00Z"
            }},
            "installation": { "uuid": "00000000-0000-0000-0000-000000000000" }
        });
        let envelope = sentry_webhook_to_envelope(&webhook).unwrap();
        let signals = merge0_adapter_sentry::SentryAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Sentry);
        assert_eq!(signals[0].source_ref, "5312345678");
        assert_eq!(signals[0].kind, SignalKind::Exception);
    }

    #[test]
    fn sentry_webhook_unrecognized_shapes_are_none() {
        assert_eq!(
            sentry_webhook_to_envelope(&json!({"action": "created"})),
            None
        );
        assert_eq!(
            sentry_webhook_to_envelope(&json!({"data": {"issue": "not-an-object"}})),
            None
        );
        assert_eq!(sentry_webhook_to_envelope(&json!([1, 2, 3])), None);
    }

    #[test]
    fn posthog_rageclick_webhook_round_trips_through_adapter() {
        let webhook = json!({
            "event": {
                "event": "$rageclick",
                "distinct_id": "user-1",
                "timestamp": "2026-08-05T14:00:00Z",
                "properties": { "$pathname": "/reports", "$session_id": "s-1" }
            },
            "person": { "id": "user-1" }
        });
        let envelope =
            posthog_webhook_to_envelope(&webhook, "https://us.posthog.com/project/1").unwrap();
        let signals = merge0_adapter_posthog::PosthogAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Posthog);
        assert_eq!(signals[0].kind, SignalKind::UxFriction);
        assert_eq!(signals[0].join_keys.url_path.as_deref(), Some("/reports"));
    }

    #[test]
    fn posthog_issue_webhook_round_trips_through_adapter() {
        let webhook = json!({
            "issue": {
                "id": "0198a5f2-1111-7aaa-bbbb-3c4d5e6f7a8b",
                "name": "TypeError",
                "description": "Cannot read properties of undefined",
                "first_seen": "2026-08-01T04:12:00Z",
                "last_seen": "2026-08-06T22:30:00Z",
                "users": 42
            }
        });
        let envelope =
            posthog_webhook_to_envelope(&webhook, "https://us.posthog.com/project/1").unwrap();
        let signals = merge0_adapter_posthog::PosthogAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, SignalKind::Exception);
        assert_eq!(signals[0].affected_count, Some(42));
    }

    #[test]
    fn posthog_webhook_unrecognized_shapes_are_none() {
        let base = "https://us.posthog.com/project/1";
        // A non-rageclick event is not an error-tracking issue either.
        assert_eq!(
            posthog_webhook_to_envelope(&json!({"event": {"event": "$pageview"}}), base),
            None
        );
        assert_eq!(
            posthog_webhook_to_envelope(&json!({"unrelated": true}), base),
            None
        );
        assert_eq!(
            posthog_webhook_to_envelope(&json!("just a string"), base),
            None
        );
    }

    #[test]
    fn zendesk_webhook_round_trips_through_adapter() {
        let ticket = json!({
            "id": 4812,
            "subject": "Sync page crashes",
            "description": "Blank page on open.",
            "priority": "urgent",
            "organization_id": 360012345,
            "created_at": "2026-08-05T14:00:00Z",
            "updated_at": "2026-08-06T09:30:00Z"
        });
        for webhook in [
            json!({ "type": "zen:event-type:ticket.created", "detail": ticket }),
            json!({ "ticket": ticket }),
        ] {
            let envelope =
                zendesk_webhook_to_envelope(&webhook, "https://acme.zendesk.com/agent").unwrap();
            let signals = merge0_adapter_zendesk::ZendeskAdapter
                .normalize(&envelope)
                .unwrap();
            assert_eq!(signals.len(), 1);
            assert_eq!(signals[0].source, Source::Zendesk);
            assert_eq!(signals[0].source_ref, "4812");
            assert_eq!(
                signals[0].join_keys.account_id.as_deref(),
                Some("360012345")
            );
        }
    }

    #[test]
    fn zendesk_webhook_unrecognized_shapes_are_none() {
        let base = "https://acme.zendesk.com/agent";
        assert_eq!(
            zendesk_webhook_to_envelope(&json!({"detail": "gone"}), base),
            None
        );
        assert_eq!(
            zendesk_webhook_to_envelope(&json!({"detail": {"no_id": true}}), base),
            None
        );
        assert_eq!(zendesk_webhook_to_envelope(&json!({}), base), None);
    }

    #[test]
    fn datadog_webhook_round_trips_through_adapter() {
        // The documented webhook-integration template variables.
        let webhook = json!({
            "id": "7654321099887766554",
            "event_title": "[Triggered] High error rate on example-api",
            "event_msg": "Error rate exceeded 5% over the last 10 minutes.",
            "alert_type": "error",
            "alert_id": "7654321",
            "date": "1786108800000",
            "tags": "env:prod, service:example-api, version:v2.3.0"
        });
        let envelope = datadog_webhook_to_envelope(&webhook, "https://app.datadoghq.com").unwrap();
        let signals = merge0_adapter_datadog::DatadogAdapter
            .normalize(&envelope)
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source, Source::Datadog);
        assert_eq!(signals[0].source_ref, "7654321099887766554");
        assert_eq!(signals[0].kind, SignalKind::Exception);
        assert_eq!(signals[0].join_keys.release.as_deref(), Some("v2.3.0"));
        assert_eq!(
            signals[0]
                .first_seen
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "2026-08-07T13:20:00Z"
        );
    }

    #[test]
    fn datadog_webhook_unrecognized_shapes_are_none() {
        let base = "https://app.datadoghq.com";
        // Missing date / title / id.
        assert_eq!(
            datadog_webhook_to_envelope(&json!({"id": "1", "title": "t"}), base),
            None
        );
        assert_eq!(
            datadog_webhook_to_envelope(&json!({"id": "1", "date": 1786108800000i64}), base),
            None
        );
        assert_eq!(
            datadog_webhook_to_envelope(&json!({"title": "t", "date": 1786108800000i64}), base),
            None
        );
        assert_eq!(
            datadog_webhook_to_envelope(
                &json!({"id": "1", "title": "t", "date": "not-a-number"}),
                base
            ),
            None
        );
    }
}
