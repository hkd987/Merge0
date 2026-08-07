//! Parsing Slack interactivity payloads into typed verdicts.
//!
//! Slack posts interactions as `application/x-www-form-urlencoded` bodies
//! with a single `payload=<url-encoded json>` field. We accept that form and
//! bare JSON (useful for tests and for callers that already decoded the
//! form). Output is a typed [`SlackVerdict`] — approve, or dismiss with one
//! of the four structured [`DismissReason`] values (PRD §6) — so downstream
//! outcome-memory writes never parse free text. Untrusted input never
//! panics; every malformed shape maps to a [`SlackError`] variant.

use crate::message::{ACTION_APPROVE, ACTION_DISMISS};
use crate::SlackError;
use merge0_signal::DismissReason;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The reviewer's decision extracted from an interaction payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackVerdict {
    pub report_id: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Dismiss(DismissReason),
}

/// Parse a Slack interactivity payload (form-encoded `payload=<json>` or
/// bare JSON) into a [`SlackVerdict`].
pub fn parse_interaction(payload: &str) -> Result<SlackVerdict, SlackError> {
    let json_text = match payload.strip_prefix("payload=") {
        // Slack sends a single form field; ignore anything after a stray '&'.
        Some(rest) => percent_decode(rest.split('&').next().unwrap_or(rest))?,
        None => payload.to_string(),
    };
    let value: Value = serde_json::from_str(&json_text)
        .map_err(|e| SlackError::InvalidPayload(format!("not JSON: {e}")))?;
    let action = value
        .get("actions")
        .and_then(|a| a.get(0))
        .ok_or_else(|| SlackError::InvalidPayload("missing actions[0]".into()))?;
    let action_id = action
        .get("action_id")
        .and_then(Value::as_str)
        .ok_or_else(|| SlackError::InvalidPayload("missing action_id".into()))?;

    match action_id {
        ACTION_APPROVE => {
            let report_id = action
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| SlackError::InvalidPayload("approve action missing value".into()))?;
            Ok(SlackVerdict {
                report_id: report_id.to_string(),
                verdict: Verdict::Approve,
            })
        }
        ACTION_DISMISS => {
            let option_value = action
                .get("selected_option")
                .and_then(|o| o.get("value"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SlackError::InvalidPayload(
                        "dismiss action missing selected_option.value".into(),
                    )
                })?;
            let (report_id, reason_str) = option_value.split_once(':').ok_or_else(|| {
                SlackError::InvalidPayload("dismiss value must be <report_id>:<reason>".into())
            })?;
            let reason = parse_dismiss_reason(reason_str)
                .ok_or_else(|| SlackError::UnknownDismissReason(reason_str.to_string()))?;
            Ok(SlackVerdict {
                report_id: report_id.to_string(),
                verdict: Verdict::Dismiss(reason),
            })
        }
        other => Err(SlackError::UnknownAction(other.to_string())),
    }
}

fn parse_dismiss_reason(s: &str) -> Option<DismissReason> {
    match s {
        "intended_behavior" => Some(DismissReason::IntendedBehavior),
        "wont_fix" => Some(DismissReason::WontFix),
        "duplicate" => Some(DismissReason::Duplicate),
        "bad_evidence" => Some(DismissReason::BadEvidence),
        _ => None,
    }
}

/// Minimal `application/x-www-form-urlencoded` value decoder ('+' is space,
/// `%XX` is a byte). Kept local to avoid a dependency for one field.
fn percent_decode(s: &str) -> Result<String, SlackError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let (Some(hi), Some(lo)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                    return Err(SlackError::InvalidPayload(
                        "truncated percent escape".into(),
                    ));
                };
                let (Some(hi), Some(lo)) = (hex_val(*hi), hex_val(*lo)) else {
                    return Err(SlackError::InvalidPayload("invalid percent escape".into()));
                };
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| SlackError::InvalidPayload("decoded payload is not UTF-8".into()))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DISMISS_REASONS;
    use serde_json::json;

    const REPORT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn approve_payload() -> String {
        json!({
            "type": "block_actions",
            "actions": [
                { "type": "button", "action_id": "approve", "value": REPORT_ID }
            ]
        })
        .to_string()
    }

    fn dismiss_payload(reason: DismissReason) -> String {
        json!({
            "type": "block_actions",
            "actions": [
                {
                    "type": "static_select",
                    "action_id": "dismiss",
                    "selected_option": {
                        "value": format!("{REPORT_ID}:{}", reason.as_str())
                    }
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn approve_round_trips_from_bare_json() {
        let verdict = parse_interaction(&approve_payload()).unwrap();
        assert_eq!(verdict.report_id, REPORT_ID);
        assert_eq!(verdict.verdict, Verdict::Approve);
    }

    #[test]
    fn approve_round_trips_from_form_encoding() {
        // URL-encode the JSON the way Slack does: reserved chars escaped.
        let encoded: String = approve_payload()
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                other => format!("%{other:02X}"),
            })
            .collect();
        let verdict = parse_interaction(&format!("payload={encoded}")).unwrap();
        assert_eq!(verdict.report_id, REPORT_ID);
        assert_eq!(verdict.verdict, Verdict::Approve);
    }

    #[test]
    fn every_dismiss_reason_round_trips() {
        for reason in DISMISS_REASONS {
            let verdict = parse_interaction(&dismiss_payload(reason)).unwrap();
            assert_eq!(verdict.report_id, REPORT_ID);
            assert_eq!(verdict.verdict, Verdict::Dismiss(reason));
        }
    }

    #[test]
    fn unknown_dismiss_reason_is_rejected() {
        let payload = json!({
            "actions": [{
                "action_id": "dismiss",
                "selected_option": { "value": format!("{REPORT_ID}:because") }
            }]
        })
        .to_string();
        assert!(matches!(
            parse_interaction(&payload),
            Err(SlackError::UnknownDismissReason(r)) if r == "because"
        ));
    }

    #[test]
    fn unknown_action_and_garbage_never_panic() {
        let payload = json!({
            "actions": [{ "action_id": "snooze", "value": REPORT_ID }]
        })
        .to_string();
        assert!(matches!(
            parse_interaction(&payload),
            Err(SlackError::UnknownAction(a)) if a == "snooze"
        ));
        assert!(matches!(
            parse_interaction("not json at all"),
            Err(SlackError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_interaction("payload=%ZZ"),
            Err(SlackError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_interaction("{\"actions\": []}"),
            Err(SlackError::InvalidPayload(_))
        ));
    }
}
