//! Generic ingestion → Signals: the published webhook integration surface
//! ([`WebhookAdapter`]) and OTLP/JSON logs ([`OtelAdapter`]).
//!
//! # `WebhookAdapter`
//!
//! Supported envelope endpoints:
//!
//! - `signals` — a bare JSON array of caller-submitted signal objects
//!   (`kind`, `severity`, `title`, `body`, `dedupe_key`, `first_seen`,
//!   `last_seen` required; `evidence`, `join_keys`, `affected_count`
//!   optional), one Signal per submission.
//!
//! No envelope context is required.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Callers describe, Merge0 identifies.** The adapter assigns the `id`,
//!   forces `source` to `webhook`, and derives the fingerprint from the
//!   caller's `dedupe_key` — submissions cannot forge another source or an
//!   arbitrary fingerprint.
//! - **`first_seen`/`last_seen` are required, with no wall-clock defaults.**
//!   Defaulting to "now" would make normalization non-deterministic and
//!   break golden conformance; callers must say when they observed the thing.
//! - **`dedupe_key` must be non-empty** — an empty key would collapse every
//!   submission into one fingerprint.
//! - **`source_ref`** is the `dedupe_key`: it is the only caller-side
//!   identifier the contract has.
//!
//! # `OtelAdapter`
//!
//! Supported envelope endpoints:
//!
//! - `otel_logs` — an OTLP/JSON logs export
//!   (`{"resourceLogs": [...]}`), one `exception` Signal per qualifying
//!   log record.
//!
//! No envelope context is required.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Only `severityText` of `FATAL`, `ERROR`, or `WARN` qualifies.**
//!   INFO/DEBUG/TRACE (and records with no `severityText`) are routine
//!   telemetry, not signals — they are skipped, not errored. Severity maps
//!   FATAL → critical, ERROR → high, WARN → medium.
//! - **`timeUnixNano` accepts both JSON string and number.** OTLP/JSON
//!   encodes int64 as a JSON *string* per the protobuf JSON mapping, but
//!   plenty of emitters send a bare number; both are accepted.
//! - **`title`** is the first line of the log body, truncated to 120
//!   characters; **`body`** is the full log body.
//! - **Fingerprint** is `["otel", <service.name>, <title>]`: OTLP log records
//!   have no vendor-side grouping id, so service + normalized first line is
//!   the closest stable identity. The `service.name` resource attribute is
//!   required for qualifying records.
//! - **`join_keys`**: `release` from the `service.version` resource
//!   attribute; `url_path` from the `url.path` (preferred) or `http.route`
//!   log attribute; `account_id` from the `enduser.id` log attribute.
//! - **`evidence` is empty** — there is no deep link into "the OTLP export".
//! - **`raw`** is the individual log record (not the whole resource batch).

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{fingerprint, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source};
use serde::Deserialize;
use ulid::Ulid;

pub struct WebhookAdapter;

/// A caller-submitted signal on the `signals` endpoint — the published
/// integration contract. Unknown fields are preserved via `raw`.
#[derive(Debug, Deserialize)]
struct Submission {
    kind: SignalKind,
    severity: Severity,
    title: String,
    body: String,
    dedupe_key: String,
    #[serde(default)]
    evidence: Vec<EvidenceLink>,
    #[serde(default)]
    join_keys: JoinKeys,
    #[serde(default)]
    affected_count: Option<u64>,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
}

impl Adapter for WebhookAdapter {
    fn source(&self) -> Source {
        Source::Webhook
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "signals" => {
                let submissions = envelope.payload.as_array().ok_or_else(|| {
                    AdapterError::Malformed("signals payload must be a JSON array".into())
                })?;
                submissions.iter().map(normalize_submission).collect()
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

fn normalize_submission(raw: &serde_json::Value) -> Result<Signal, AdapterError> {
    let submission: Submission = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid webhook submission: {e}")))?;
    if submission.dedupe_key.is_empty() {
        return Err(AdapterError::Malformed(
            "dedupe_key must not be empty".into(),
        ));
    }

    Ok(Signal {
        id: Ulid::generate(),
        source: Source::Webhook,
        source_ref: submission.dedupe_key.clone(),
        kind: submission.kind,
        severity: submission.severity,
        title: submission.title.clone(),
        body: submission.body.clone(),
        evidence: submission.evidence.clone(),
        fingerprint: fingerprint(Source::Webhook, &["custom", &submission.dedupe_key]),
        join_keys: submission.join_keys.clone(),
        affected_count: submission.affected_count,
        delegated: false,
        first_seen: submission.first_seen,
        last_seen: submission.last_seen,
        raw: raw.clone(),
    })
}

pub struct OtelAdapter;

/// OTLP/JSON logs payload — only the fields we normalize; each log record is
/// preserved verbatim via `raw`.
#[derive(Debug, Deserialize)]
struct OtlpLogs {
    #[serde(rename = "resourceLogs")]
    resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Deserialize)]
struct ResourceLogs {
    #[serde(default)]
    resource: Resource,
    #[serde(rename = "scopeLogs", default)]
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Default, Deserialize)]
struct Resource {
    #[serde(default)]
    attributes: Vec<KeyValue>,
}

#[derive(Debug, Deserialize)]
struct ScopeLogs {
    #[serde(rename = "logRecords", default)]
    log_records: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct LogRecord {
    #[serde(rename = "timeUnixNano")]
    time_unix_nano: UnixNano,
    #[serde(rename = "severityText", default)]
    severity_text: Option<String>,
    #[serde(default)]
    body: Option<AnyValue>,
    #[serde(default)]
    attributes: Vec<KeyValue>,
}

/// OTLP `KeyValue` — `[{"key": ..., "value": {"stringValue": ...}}]`. Only
/// string values participate in normalization.
#[derive(Debug, Deserialize)]
struct KeyValue {
    key: String,
    #[serde(default)]
    value: AnyValue,
}

#[derive(Debug, Default, Deserialize)]
struct AnyValue {
    #[serde(rename = "stringValue", default)]
    string_value: Option<String>,
}

/// OTLP/JSON encodes int64 as a JSON string (protobuf JSON mapping); many
/// emitters send a bare number instead. Accept both (module docs).
#[derive(Debug, Clone, Copy)]
struct UnixNano(u64);

impl<'de> Deserialize<'de> for UnixNano {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Num(u64),
            Str(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Num(nanos) => Ok(UnixNano(nanos)),
            Repr::Str(text) => text
                .parse::<u64>()
                .map(UnixNano)
                .map_err(|e| serde::de::Error::custom(format!("timeUnixNano is not a u64: {e}"))),
        }
    }
}

impl Adapter for OtelAdapter {
    fn source(&self) -> Source {
        Source::Otel
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;

        match envelope.endpoint.as_str() {
            "otel_logs" => {
                let logs: OtlpLogs =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!(
                            "expected {{\"resourceLogs\": [...]}}: {e}"
                        ))
                    })?;
                let mut signals = vec![];
                for resource_logs in &logs.resource_logs {
                    for scope_logs in &resource_logs.scope_logs {
                        for raw in &scope_logs.log_records {
                            if let Some(signal) =
                                normalize_log_record(raw, &resource_logs.resource)?
                            {
                                signals.push(signal);
                            }
                        }
                    }
                }
                Ok(signals)
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one log record; `Ok(None)` means "skipped by design" (module
/// docs), which is distinct from malformed input.
fn normalize_log_record(
    raw: &serde_json::Value,
    resource: &Resource,
) -> Result<Option<Signal>, AdapterError> {
    let record: LogRecord = serde_json::from_value(raw.clone())
        .map_err(|e| AdapterError::Malformed(format!("invalid otel log record: {e}")))?;

    let severity = match record.severity_text.as_deref() {
        Some("FATAL") => Severity::Critical,
        Some("ERROR") => Severity::High,
        Some("WARN") => Severity::Medium,
        // INFO/DEBUG/TRACE, absent, or anything else: not a signal.
        _ => return Ok(None),
    };

    let body = record
        .body
        .as_ref()
        .and_then(|b| b.string_value.clone())
        .ok_or_else(|| AdapterError::Malformed("otel log record has no body.stringValue".into()))?;
    let title = truncate_chars(body.lines().next().unwrap_or_default(), 120);

    let service_name = attr(&resource.attributes, "service.name").ok_or_else(|| {
        AdapterError::Malformed("resource is missing the service.name attribute".into())
    })?;

    let nanos = i64::try_from(record.time_unix_nano.0)
        .map_err(|_| AdapterError::Malformed("timeUnixNano out of range".into()))?;
    let timestamp = DateTime::<Utc>::from_timestamp_nanos(nanos);

    let url_path =
        attr(&record.attributes, "url.path").or_else(|| attr(&record.attributes, "http.route"));

    Ok(Some(Signal {
        id: Ulid::generate(),
        source: Source::Otel,
        source_ref: format!("{service_name}:{}", record.time_unix_nano.0),
        kind: SignalKind::Exception,
        severity,
        title: title.clone(),
        body,
        evidence: vec![],
        fingerprint: fingerprint(Source::Otel, &["otel", &service_name, &title]),
        join_keys: JoinKeys {
            release: attr(&resource.attributes, "service.version"),
            account_id: attr(&record.attributes, "enduser.id"),
            url_path,
            ..Default::default()
        },
        affected_count: None,
        delegated: false,
        first_seen: timestamp,
        last_seen: timestamp,
        raw: raw.clone(),
    }))
}

/// Look up a string attribute in an OTLP KeyValue list.
fn attr(attributes: &[KeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.string_value.clone())
}

/// Truncate to at most `max` characters on a char boundary.
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- WebhookAdapter ---

    fn submission() -> serde_json::Value {
        serde_json::json!({
            "kind": "regression",
            "severity": "high",
            "title": "Checkout conversion dropped 8%",
            "body": "Conversion on /checkout dropped from 61% to 53% after the last deploy.",
            "dedupe_key": "checkout-conversion-drop",
            "first_seen": "2026-08-05T14:00:00Z",
            "last_seen": "2026-08-06T14:00:00Z"
        })
    }

    fn webhook_envelope(payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "endpoint": "signals", "payload": payload })
    }

    #[test]
    fn submission_fields_pass_through() {
        let mut full = submission();
        full["evidence"] = serde_json::json!([{
            "kind": "other",
            "label": "Dashboard",
            "url": "https://metrics.example.com/checkout"
        }]);
        full["join_keys"] = serde_json::json!({ "release": "v2.3.0", "url_path": "/checkout" });
        full["affected_count"] = serde_json::json!(240);
        let signals = WebhookAdapter
            .normalize(&webhook_envelope(serde_json::json!([full])))
            .unwrap();
        let signal = &signals[0];
        assert_eq!(signal.kind, SignalKind::Regression);
        assert_eq!(signal.severity, Severity::High);
        assert_eq!(signal.source, Source::Webhook);
        assert_eq!(signal.source_ref, "checkout-conversion-drop");
        assert_eq!(signal.evidence.len(), 1);
        assert_eq!(signal.join_keys.release.as_deref(), Some("v2.3.0"));
        assert_eq!(signal.affected_count, Some(240));
    }

    #[test]
    fn webhook_fingerprint_depends_only_on_dedupe_key() {
        let mut variant = submission();
        variant["title"] = serde_json::json!("A different title");
        variant["severity"] = serde_json::json!("low");
        variant["last_seen"] = serde_json::json!("2026-08-07T14:00:00Z");
        let a = &WebhookAdapter
            .normalize(&webhook_envelope(serde_json::json!([submission()])))
            .unwrap()[0];
        let b = &WebhookAdapter
            .normalize(&webhook_envelope(serde_json::json!([variant])))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);

        let mut other = submission();
        other["dedupe_key"] = serde_json::json!("another-key");
        let c = &WebhookAdapter
            .normalize(&webhook_envelope(serde_json::json!([other])))
            .unwrap()[0];
        assert_ne!(a.fingerprint, c.fingerprint);
    }

    #[test]
    fn missing_timestamps_are_an_error_not_defaulted() {
        // Determinism: no wall-clock defaults (module docs).
        for field in ["first_seen", "last_seen"] {
            let mut incomplete = submission();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(matches!(
                WebhookAdapter.normalize(&webhook_envelope(serde_json::json!([incomplete]))),
                Err(AdapterError::Malformed(_))
            ));
        }
    }

    #[test]
    fn empty_dedupe_key_is_rejected() {
        let mut bad = submission();
        bad["dedupe_key"] = serde_json::json!("");
        assert!(matches!(
            WebhookAdapter.normalize(&webhook_envelope(serde_json::json!([bad]))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_submission_fields_are_ignored_but_preserved_in_raw() {
        let mut extra = submission();
        extra["some_caller_field"] = serde_json::json!({ "nested": true });
        let signals = WebhookAdapter
            .normalize(&webhook_envelope(serde_json::json!([extra])))
            .unwrap();
        assert_eq!(
            signals[0].raw["some_caller_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn webhook_rejects_other_endpoints() {
        let input = serde_json::json!({ "endpoint": "otel_logs", "payload": [] });
        assert!(matches!(
            WebhookAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }

    // --- OtelAdapter ---

    fn otel_envelope(records: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": "otel_logs",
            "payload": {
                "resourceLogs": [{
                    "resource": {
                        "attributes": [
                            { "key": "service.name", "value": { "stringValue": "chalk-api" } },
                            { "key": "service.version", "value": { "stringValue": "v2.3.0" } }
                        ]
                    },
                    "scopeLogs": [{ "logRecords": records }]
                }]
            }
        })
    }

    fn error_record() -> serde_json::Value {
        serde_json::json!({
            "timeUnixNano": "1785938400000000000",
            "severityText": "ERROR",
            "body": { "stringValue": "NullPointerException in RosterService\n  at RosterService.load" }
        })
    }

    #[test]
    fn otel_severity_mapping() {
        let records = serde_json::json!([
            { "timeUnixNano": "1785938400000000000", "severityText": "FATAL",
              "body": { "stringValue": "fatal" } },
            { "timeUnixNano": "1785938401000000000", "severityText": "ERROR",
              "body": { "stringValue": "error" } },
            { "timeUnixNano": "1785938402000000000", "severityText": "WARN",
              "body": { "stringValue": "warn" } }
        ]);
        let signals = OtelAdapter.normalize(&otel_envelope(records)).unwrap();
        assert_eq!(signals.len(), 3);
        assert_eq!(signals[0].severity, Severity::Critical);
        assert_eq!(signals[1].severity, Severity::High);
        assert_eq!(signals[2].severity, Severity::Medium);
    }

    #[test]
    fn info_debug_trace_and_absent_severity_are_skipped() {
        let records = serde_json::json!([
            { "timeUnixNano": "1785938400000000000", "severityText": "INFO",
              "body": { "stringValue": "info" } },
            { "timeUnixNano": "1785938401000000000", "severityText": "DEBUG",
              "body": { "stringValue": "debug" } },
            { "timeUnixNano": "1785938402000000000", "severityText": "TRACE",
              "body": { "stringValue": "trace" } },
            { "timeUnixNano": "1785938403000000000",
              "body": { "stringValue": "no severity" } },
            error_record()
        ]);
        let signals = OtelAdapter.normalize(&otel_envelope(records)).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].severity, Severity::High);
    }

    #[test]
    fn time_unix_nano_accepts_string_and_number() {
        // OTLP/JSON int64-as-string vs. bare number: identical output.
        let mut as_number = error_record();
        as_number["timeUnixNano"] = serde_json::json!(1785938400000000000u64);
        let a = &OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([error_record()])))
            .unwrap()[0];
        let b = &OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([as_number])))
            .unwrap()[0];
        assert_eq!(a.first_seen, b.first_seen);
        assert_eq!(a.source_ref, b.source_ref);
        assert_eq!(a.first_seen.to_rfc3339(), "2026-08-05T14:00:00+00:00");

        let mut bad = error_record();
        bad["timeUnixNano"] = serde_json::json!("not-a-number");
        assert!(matches!(
            OtelAdapter.normalize(&otel_envelope(serde_json::json!([bad]))),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn title_is_first_line_truncated_to_120_chars() {
        let long_first_line = "x".repeat(200);
        let record = serde_json::json!({
            "timeUnixNano": "1785938400000000000",
            "severityText": "ERROR",
            "body": { "stringValue": format!("{long_first_line}\nsecond line") }
        });
        let signals = OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([record])))
            .unwrap();
        assert_eq!(signals[0].title, "x".repeat(120));
        assert!(signals[0].body.contains("second line"));
    }

    #[test]
    fn fingerprint_stable_across_timestamps() {
        let mut later = error_record();
        later["timeUnixNano"] = serde_json::json!("1786024800000000000");
        let a = &OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([error_record()])))
            .unwrap()[0];
        let b = &OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([later])))
            .unwrap()[0];
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.source_ref, b.source_ref);
    }

    #[test]
    fn join_keys_from_resource_and_log_attributes() {
        let mut record = error_record();
        record["attributes"] = serde_json::json!([
            { "key": "http.route", "value": { "stringValue": "/districts/:id" } },
            { "key": "enduser.id", "value": { "stringValue": "user-77" } }
        ]);
        let signals = OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([record])))
            .unwrap();
        let keys = &signals[0].join_keys;
        assert_eq!(keys.release.as_deref(), Some("v2.3.0"));
        assert_eq!(keys.url_path.as_deref(), Some("/districts/:id"));
        assert_eq!(keys.account_id.as_deref(), Some("user-77"));

        // url.path wins over http.route when both are present.
        let mut record = error_record();
        record["attributes"] = serde_json::json!([
            { "key": "http.route", "value": { "stringValue": "/districts/:id" } },
            { "key": "url.path", "value": { "stringValue": "/districts/42" } }
        ]);
        let signals = OtelAdapter
            .normalize(&otel_envelope(serde_json::json!([record])))
            .unwrap();
        assert_eq!(
            signals[0].join_keys.url_path.as_deref(),
            Some("/districts/42")
        );
    }

    #[test]
    fn missing_service_name_is_an_error_for_qualifying_records() {
        let input = serde_json::json!({
            "endpoint": "otel_logs",
            "payload": {
                "resourceLogs": [{
                    "resource": { "attributes": [] },
                    "scopeLogs": [{ "logRecords": [error_record()] }]
                }]
            }
        });
        assert!(matches!(
            OtelAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn otel_rejects_other_endpoints() {
        let input = serde_json::json!({ "endpoint": "signals", "payload": [] });
        assert!(matches!(
            OtelAdapter.normalize(&input),
            Err(AdapterError::UnsupportedEndpoint(_))
        ));
    }
}
