//! Trello → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `cards` — the Trello list-cards API response (a **bare JSON array** of
//!   cards, `[...]` — Trello does not wrap list responses), one `ticket`
//!   Signal per open card.
//!
//! Envelope context: `{}` — nothing is needed; Trello cards carry their own
//! `shortUrl` deep link.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Closed (archived) cards are skipped** (`closed: true` → no Signal). An
//!   archived card is resolved or abandoned work, not an actionable signal.
//! - **Severity** maps from card label names — label conventions are how
//!   Trello boards encode urgency, since cards have no priority field. Any
//!   label named (case-insensitive) `critical` → critical; else any `urgent`
//!   or `high` → high; else any `bug` → medium; else low (an unlabeled card
//!   has not demonstrated urgency).
//! - **Evidence** is the card's `shortUrl` (label `Trello card {name}` with
//!   the name truncated to 40 characters so evidence labels stay scannable,
//!   kind `ticket`).
//! - **`last_seen`** is `dateLastActivity`; **`first_seen`** is the card's
//!   `start` date when present, else `dateLastActivity` — Trello cards do not
//!   expose a creation timestamp in the list-cards payload, so `start` is the
//!   earliest honest date we have.
//! - **`body`** is the card `desc`; absent/empty desc → empty string.
//! - **`join_keys`**: none — a Trello card carries no release, stack,
//!   account, or URL identity we could derive without inventing fields.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct TrelloAdapter;

/// A Trello card — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Card {
    id: String,
    name: String,
    #[serde(default)]
    desc: String,
    date_last_activity: DateTime<Utc>,
    #[serde(default)]
    start: Option<DateTime<Utc>>,
    #[serde(default)]
    closed: bool,
    short_url: String,
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
struct Label {
    #[serde(default)]
    name: String,
}

impl Adapter for TrelloAdapter {
    fn source(&self) -> Source {
        Source::Trello
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "cards" => {
                let cards: Vec<serde_json::Value> =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected a bare array of cards: {e}"))
                    })?;
                let mut signals = Vec::new();
                for card in &cards {
                    if let Some(signal) = normalize_card(card)? {
                        signals.push(signal);
                    }
                }
                Ok(signals)
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one card; `Ok(None)` means "deliberately skipped" (closed card),
/// which is distinct from `Err` (malformed input).
fn normalize_card(raw: &serde_json::Value) -> Result<Option<Signal>, AdapterError> {
    let card: Card = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid trello card: {e}")))?;
    if card.closed {
        return Ok(None);
    }
    let id = card.id;

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Trello,
        source_ref: id.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_labels(&card.labels),
        title: card.name.clone(),
        body: card.desc,
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Trello card {}", truncate_chars(&card.name, 40)),
            url: card.short_url,
        }],
        fingerprint: fingerprint(Source::Trello, &[&id]),
        join_keys: JoinKeys::default(),
        affected_count: None,
        delegated: false,
        first_seen: card.start.unwrap_or(card.date_last_activity),
        last_seen: card.date_last_activity,
        raw: raw.clone(),
    }))
}

/// Label names → severity (see module docs). Tiers are checked in descending
/// order so the most urgent label wins when several conventions coexist.
fn severity_from_labels(labels: &[Label]) -> Severity {
    let has = |wanted: &[&str]| {
        labels
            .iter()
            .any(|label| wanted.iter().any(|w| label.name.eq_ignore_ascii_case(w)))
    };
    if has(&["critical"]) {
        Severity::Critical
    } else if has(&["urgent", "high"]) {
        Severity::High
    } else if has(&["bug"]) {
        Severity::Medium
    } else {
        Severity::Low
    }
}

/// Truncate to at most `max` characters (chars, not bytes, so multi-byte
/// names never split mid-codepoint).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<Label> {
        names
            .iter()
            .map(|n| Label {
                name: (*n).to_string(),
            })
            .collect()
    }

    #[test]
    fn label_severity_mapping() {
        assert_eq!(
            severity_from_labels(&labels(&["critical"])),
            Severity::Critical
        );
        assert_eq!(
            severity_from_labels(&labels(&["CRITICAL"])),
            Severity::Critical
        );
        assert_eq!(severity_from_labels(&labels(&["urgent"])), Severity::High);
        assert_eq!(severity_from_labels(&labels(&["High"])), Severity::High);
        assert_eq!(severity_from_labels(&labels(&["Bug"])), Severity::Medium);
        assert_eq!(severity_from_labels(&labels(&["design"])), Severity::Low);
        assert_eq!(severity_from_labels(&labels(&[])), Severity::Low);
        // Most urgent label wins.
        assert_eq!(
            severity_from_labels(&labels(&["bug", "urgent", "Critical"])),
            Severity::Critical
        );
        assert_eq!(
            severity_from_labels(&labels(&["bug", "high"])),
            Severity::High
        );
        // "named", not "contains": near-misses do not match.
        assert_eq!(
            severity_from_labels(&labels(&["critical-path", "not urgent"])),
            Severity::Low
        );
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": {},
            "payload": payload,
        })
    }

    fn minimal_card() -> serde_json::Value {
        serde_json::json!({
            "id": "64f1c0ffee0badc0de000001",
            "name": "Fix broken export button",
            "closed": false,
            "dateLastActivity": "2026-08-06T09:00:00Z",
            "shortUrl": "https://trello.com/c/aBcD1234"
        })
    }

    fn normalize(payload: serde_json::Value) -> Vec<Signal> {
        TrelloAdapter
            .normalize(&envelope("cards", payload))
            .unwrap()
    }

    #[test]
    fn closed_cards_are_skipped() {
        let mut closed = minimal_card();
        closed["id"] = serde_json::json!("64f1c0ffee0badc0de000099");
        closed["closed"] = serde_json::json!(true);
        let signals = normalize(serde_json::json!([closed, minimal_card()]));
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "64f1c0ffee0badc0de000001");
    }

    #[test]
    fn first_seen_prefers_start_else_last_activity() {
        let mut card = minimal_card();
        card["start"] = serde_json::json!("2026-08-01T08:00:00Z");
        let signals = normalize(serde_json::json!([card]));
        assert_eq!(
            signals[0].first_seen,
            "2026-08-01T08:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            signals[0].last_seen,
            "2026-08-06T09:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );

        let signals = normalize(serde_json::json!([minimal_card()]));
        assert_eq!(signals[0].first_seen, signals[0].last_seen);
    }

    #[test]
    fn evidence_label_truncates_long_names_to_40_chars() {
        let mut card = minimal_card();
        card["name"] =
            serde_json::json!("A very long card name that keeps going well past forty characters");
        let signals = normalize(serde_json::json!([card]));
        assert_eq!(
            signals[0].evidence[0].label,
            "Trello card A very long card name that keeps going w"
        );
        assert_eq!(signals[0].evidence[0].kind, EvidenceKind::Ticket);
        assert_eq!(signals[0].evidence[0].url, "https://trello.com/c/aBcD1234");
        // Title is NOT truncated — only the evidence label is.
        assert_eq!(
            signals[0].title,
            "A very long card name that keeps going well past forty characters"
        );
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        let mut updated = minimal_card();
        updated["dateLastActivity"] = serde_json::json!("2026-08-07T12:00:00Z");
        updated["labels"] = serde_json::json!([{ "name": "critical" }]);
        let a = &normalize(serde_json::json!([minimal_card()]))[0];
        let b = &normalize(serde_json::json!([updated]))[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.severity, b.severity);
    }

    #[test]
    fn malformed_card_is_an_error_not_a_panic() {
        // Missing required `shortUrl`.
        let card = serde_json::json!({
            "id": "64f1c0ffee0badc0de000001",
            "name": "Fix broken export button",
            "closed": false,
            "dateLastActivity": "2026-08-06T09:00:00Z"
        });
        assert!(matches!(
            TrelloAdapter.normalize(&envelope("cards", serde_json::json!([card]))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn malformed_payload_shape_is_an_error() {
        // Payload is an object, not the bare array Trello returns.
        assert!(matches!(
            TrelloAdapter.normalize(&envelope("cards", serde_json::json!({ "cards": [] }))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let result = TrelloAdapter.normalize(&envelope("boards", serde_json::json!([])));
        match result {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "boards"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut card = minimal_card();
        card["someFutureTrelloField"] = serde_json::json!({ "nested": true });
        let signals = normalize(serde_json::json!([card]));
        assert_eq!(
            signals[0].raw["someFutureTrelloField"]["nested"],
            serde_json::Value::Bool(true)
        );
    }
}
