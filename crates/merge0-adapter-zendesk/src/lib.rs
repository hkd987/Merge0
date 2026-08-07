//! Zendesk → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `tickets` — the Zendesk list-tickets API response
//!   (`{"tickets": [...]}`), one `ticket` Signal per ticket.
//!
//! Envelope context: `{"agent_base_url": "https://acme.zendesk.com/agent"}`
//! — used to build agent-workspace deep links.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Severity** maps from the Zendesk `priority`: urgent → critical,
//!   high → high, normal → medium, low → low. Absent (priority is optional on
//!   Zendesk tickets) or unrecognized priority → low: an untriaged ticket has
//!   not demonstrated urgency yet, and a quiet inbox that's right beats a
//!   busy one.
//! - **`join_keys.account_id`** prefers `organization_id` (the vendor-side
//!   account) over `requester_id` (an individual user); both are numeric in
//!   the Zendesk API and are normalized to strings.
//! - **`first_seen`/`last_seen`** come from `created_at`/`updated_at` — a
//!   ticket "lives" from filing to last activity.
//! - **`body`** is the ticket `description` (the first comment); absent
//!   description → empty string.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct ZendeskAdapter;

/// Typed context for the Zendesk envelope.
#[derive(Debug, Deserialize)]
struct Context {
    agent_base_url: String,
}

#[derive(Debug, Deserialize)]
struct TicketsPage {
    tickets: Vec<serde_json::Value>,
}

/// A Zendesk ticket — only the fields we normalize; everything else is
/// preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Ticket {
    id: u64,
    subject: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    organization_id: Option<u64>,
    #[serde(default)]
    requester_id: Option<u64>,
}

impl Adapter for ZendeskAdapter {
    fn source(&self) -> Source {
        Source::Zendesk
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid zendesk context: {e}")))?;
        let base_url = context.agent_base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "tickets" => {
                let page: TicketsPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("expected {{\"tickets\": [...]}}: {e}"))
                    })?;
                page.tickets
                    .iter()
                    .map(|ticket| normalize_ticket(ticket, &base_url))
                    .collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_ticket(raw: &serde_json::Value, base_url: &str) -> Result<Signal, AdapterError> {
    let ticket: Ticket = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid zendesk ticket: {e}")))?;
    let id = ticket.id.to_string();

    let account_id = ticket
        .organization_id
        .or(ticket.requester_id)
        .map(|n| n.to_string());

    Ok(Signal {
        id: Ulid::new(),
        source: Source::Zendesk,
        source_ref: id.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_priority(ticket.priority.as_deref()),
        title: ticket.subject.clone(),
        body: ticket.description.clone().unwrap_or_default(),
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Ticket,
            label: format!("Zendesk ticket #{id}"),
            url: format!("{base_url}/tickets/{id}"),
        }],
        fingerprint: fingerprint(Source::Zendesk, &["ticket", &id]),
        join_keys: JoinKeys {
            account_id,
            ..Default::default()
        },
        affected_count: None,
        first_seen: ticket.created_at,
        last_seen: ticket.updated_at,
        raw: raw.clone(),
    })
}

/// Zendesk `priority` → severity (see module docs).
fn severity_from_priority(priority: Option<&str>) -> Severity {
    match priority {
        Some("urgent") => Severity::Critical,
        Some("high") => Severity::High,
        Some("normal") => Severity::Medium,
        _ => Severity::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_mapping() {
        assert_eq!(severity_from_priority(Some("urgent")), Severity::Critical);
        assert_eq!(severity_from_priority(Some("high")), Severity::High);
        assert_eq!(severity_from_priority(Some("normal")), Severity::Medium);
        assert_eq!(severity_from_priority(Some("low")), Severity::Low);
        assert_eq!(severity_from_priority(Some("unheard-of")), Severity::Low);
        assert_eq!(severity_from_priority(None), Severity::Low);
    }

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "agent_base_url": "https://acme.zendesk.com/agent/" },
            "payload": payload,
        })
    }

    fn minimal_ticket() -> serde_json::Value {
        serde_json::json!({
            "id": 3101,
            "subject": "Roster sync stuck at 90%",
            "created_at": "2026-08-05T14:00:00Z",
            "updated_at": "2026-08-06T09:00:00Z"
        })
    }

    #[test]
    fn fingerprint_stable_across_payload_variants() {
        // Same ticket id, different priority/updated_at → same fingerprint.
        let mut updated = minimal_ticket();
        updated["priority"] = serde_json::json!("urgent");
        updated["updated_at"] = serde_json::json!("2026-08-07T09:00:00Z");
        let a = &ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [minimal_ticket()] }),
            ))
            .unwrap()[0];
        let b = &ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [updated] }),
            ))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        // ...while volatile facts still differ.
        assert_ne!(a.severity, b.severity);
    }

    #[test]
    fn account_id_prefers_organization_over_requester() {
        let mut ticket = minimal_ticket();
        ticket["organization_id"] = serde_json::json!(360012345);
        ticket["requester_id"] = serde_json::json!(900222333);
        let signals = ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [ticket] }),
            ))
            .unwrap();
        assert_eq!(
            signals[0].join_keys.account_id.as_deref(),
            Some("360012345")
        );

        let mut ticket = minimal_ticket();
        ticket["requester_id"] = serde_json::json!(900222333);
        let signals = ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [ticket] }),
            ))
            .unwrap();
        assert_eq!(
            signals[0].join_keys.account_id.as_deref(),
            Some("900222333")
        );

        let signals = ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [minimal_ticket()] }),
            ))
            .unwrap();
        assert_eq!(signals[0].join_keys.account_id, None);
    }

    #[test]
    fn evidence_deep_link_uses_agent_base_url() {
        let signals = ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [minimal_ticket()] }),
            ))
            .unwrap();
        assert_eq!(
            signals[0].evidence[0].url,
            "https://acme.zendesk.com/agent/tickets/3101"
        );
    }

    #[test]
    fn malformed_ticket_is_an_error_not_a_panic() {
        // Missing required `subject`.
        let ticket = serde_json::json!({
            "id": 3101,
            "created_at": "2026-08-05T14:00:00Z",
            "updated_at": "2026-08-06T09:00:00Z"
        });
        assert!(matches!(
            ZendeskAdapter.normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [ticket] })
            )),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut ticket = minimal_ticket();
        ticket["some_future_zendesk_field"] = serde_json::json!({ "nested": true });
        let signals = ZendeskAdapter
            .normalize(&envelope(
                "tickets",
                serde_json::json!({ "tickets": [ticket] }),
            ))
            .unwrap();
        assert_eq!(
            signals[0].raw["some_future_zendesk_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("users", serde_json::json!({ "tickets": [] }));
        assert!(matches!(
            ZendeskAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
