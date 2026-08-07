//! Slack request signature verification (v0 HMAC-SHA256 scheme).
//!
//! `now` is a parameter, not a wall-clock read, so expiry behavior is unit
//! testable. Comparison is constant-time; timestamps more than five minutes
//! from `now` (in either direction — replay or clock skew) are rejected
//! before any HMAC work.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Maximum accepted distance between the request timestamp and `now`,
/// per Slack's replay-protection guidance.
pub const SIGNATURE_MAX_AGE_SECS: i64 = 5 * 60;

/// Verify a Slack request signature.
///
/// `timestamp` is the `X-Slack-Request-Timestamp` header, `signature` the
/// `X-Slack-Signature` header (`"v0=<hex>"`). Returns `false` — never an
/// error, never a panic — for anything malformed, expired, or mismatched.
pub fn verify_slack_signature(
    signing_secret: &str,
    timestamp: &str,
    body: &str,
    signature: &str,
    now: DateTime<Utc>,
) -> bool {
    let Ok(ts) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now.timestamp() - ts).abs() > SIGNATURE_MAX_AGE_SECS {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(format!("v0:{timestamp}:{body}").as_bytes());
    let expected = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
    constant_time_eq(expected.as_bytes(), signature.as_bytes())
}

/// Byte-wise constant-time equality: accumulate XOR differences with OR so
/// runtime does not depend on where the first mismatch occurs. (Length is
/// checked first; leaking the length of a hex-encoded MAC is not sensitive.)
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";

    fn sign(secret: &str, timestamp: &str, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("v0:{timestamp}:{body}").as_bytes());
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    #[test]
    fn accepts_a_valid_signature() {
        let ts = now().timestamp().to_string();
        let body = "payload=%7B%22type%22%3A%22block_actions%22%7D";
        let sig = sign(SECRET, &ts, body);
        assert!(verify_slack_signature(SECRET, &ts, body, &sig, now()));
    }

    #[test]
    fn rejects_wrong_signature_wrong_secret_and_tampered_body() {
        let ts = now().timestamp().to_string();
        let body = "payload=x";
        let sig = sign(SECRET, &ts, body);
        assert!(!verify_slack_signature(
            SECRET,
            &ts,
            body,
            "v0=deadbeef",
            now()
        ));
        assert!(!verify_slack_signature(
            "other-secret",
            &ts,
            body,
            &sig,
            now()
        ));
        assert!(!verify_slack_signature(
            SECRET,
            &ts,
            "payload=y",
            &sig,
            now()
        ));
    }

    #[test]
    fn rejects_expired_and_malformed_timestamps() {
        let body = "payload=x";
        let stale = (now().timestamp() - SIGNATURE_MAX_AGE_SECS - 1).to_string();
        let sig = sign(SECRET, &stale, body);
        assert!(
            !verify_slack_signature(SECRET, &stale, body, &sig, now()),
            "signature older than 5 minutes must be rejected even if valid"
        );

        // Exactly at the boundary is still accepted.
        let edge = (now().timestamp() - SIGNATURE_MAX_AGE_SECS).to_string();
        let sig = sign(SECRET, &edge, body);
        assert!(verify_slack_signature(SECRET, &edge, body, &sig, now()));

        let sig = sign(SECRET, "not-a-number", body);
        assert!(!verify_slack_signature(
            SECRET,
            "not-a-number",
            body,
            &sig,
            now()
        ));
    }
}
