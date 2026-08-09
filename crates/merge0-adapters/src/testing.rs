//! Golden-payload conformance harness.
//!
//! Usage from an adapter crate's tests:
//!
//! ```ignore
//! merge0_adapters::testing::check_golden_files(
//!     &PosthogAdapter,
//!     concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/issues_typical.json"),
//!     concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/issues_typical.expected.json"),
//! );
//! ```
//!
//! Expected files contain the serialized Signal array with every volatile
//! field replaced by a placeholder: `"id": "<ulid>"`. The harness verifies
//! the real value was a valid ULID before substituting the placeholder, so
//! the comparison stays deterministic without hiding a broken id.
//!
//! **Blessing:** run tests with `MERGE0_BLESS=1` to (re)write the expected
//! files from actual adapter output instead of comparing. Review the diff
//! before committing — a blessed file is a spec change.

use crate::Adapter;
use ulid::Ulid;

/// Placeholder used in `.expected.json` files for adapter-assigned ULIDs.
pub const ULID_PLACEHOLDER: &str = "<ulid>";

/// File-based golden check with bless support (see module docs).
pub fn check_golden_files(adapter: &dyn Adapter, fixture_path: &str, expected_path: &str) {
    let fixture_json = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("cannot read fixture {fixture_path}: {e}"));
    let actual = normalized_actual(adapter, &fixture_json);

    if std::env::var_os("MERGE0_BLESS").is_some() {
        let mut pretty = serde_json::to_string_pretty(&actual).unwrap();
        pretty.push('\n');
        std::fs::write(expected_path, pretty)
            .unwrap_or_else(|e| panic!("cannot write {expected_path}: {e}"));
        return;
    }

    let expected_json = std::fs::read_to_string(expected_path).unwrap_or_else(|e| {
        panic!("cannot read expected file {expected_path} (run with MERGE0_BLESS=1 to create): {e}")
    });
    let expected: serde_json::Value =
        serde_json::from_str(&expected_json).expect("expected file is not valid JSON");
    compare(&actual, &expected);
}

/// String-based golden check (no bless support) — useful for inline tests.
pub fn check_golden(adapter: &dyn Adapter, fixture_json: &str, expected_json: &str) {
    let actual = normalized_actual(adapter, fixture_json);
    let expected: serde_json::Value =
        serde_json::from_str(expected_json).expect("expected JSON is not valid");
    compare(&actual, &expected);
}

fn compare(actual: &serde_json::Value, expected: &serde_json::Value) {
    if actual != expected {
        panic!(
            "golden mismatch.\n--- actual (ids normalized) ---\n{}\n--- expected ---\n{}\n",
            serde_json::to_string_pretty(actual).unwrap(),
            serde_json::to_string_pretty(expected).unwrap(),
        );
    }
}

/// Run the fixture through the adapter, assert schema invariants every
/// conformant adapter must uphold, and return the serialized signals with
/// volatile fields normalized.
fn normalized_actual(adapter: &dyn Adapter, fixture_json: &str) -> serde_json::Value {
    let input: serde_json::Value =
        serde_json::from_str(fixture_json).expect("fixture is not valid JSON");
    let signals = adapter
        .normalize(&input)
        .expect("adapter returned an error for a golden fixture");

    let source_prefix = format!("{}:", adapter.source().as_str());
    for signal in &signals {
        assert_eq!(
            signal.source,
            adapter.source(),
            "signal source must match adapter source"
        );
        assert!(
            signal.fingerprint.starts_with(&source_prefix),
            "fingerprint {:?} must be prefixed with {:?}",
            signal.fingerprint,
            source_prefix
        );
        assert!(
            !signal.source_ref.is_empty(),
            "source_ref must not be empty"
        );
        assert!(
            !signal.raw.is_null(),
            "raw must preserve the original payload"
        );
        assert!(
            signal.first_seen <= signal.last_seen,
            "first_seen must be <= last_seen"
        );
    }

    let mut actual = serde_json::to_value(&signals).expect("signals must serialize");
    normalize_ids(&mut actual);
    actual
}

/// Replace each signal's `id` with [`ULID_PLACEHOLDER`], asserting it was a
/// valid ULID first.
fn normalize_ids(signals: &mut serde_json::Value) {
    let array = signals
        .as_array_mut()
        .expect("serialized signals must be a JSON array");
    for signal in array {
        let id = signal
            .get("id")
            .and_then(|v| v.as_str())
            .expect("signal must have a string id");
        assert!(
            Ulid::from_string(id).is_ok(),
            "signal id {id:?} is not a valid ULID"
        );
        signal["id"] = serde_json::Value::String(ULID_PLACEHOLDER.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Adapter, AdapterError};
    use merge0_signal::{JoinKeys, Severity, Signal, SignalKind, Source};

    /// A trivial adapter: `{"items": ["ref", ...]}` → one Signal per ref.
    struct FakeAdapter;

    impl Adapter for FakeAdapter {
        fn source(&self) -> Source {
            Source::Webhook
        }

        fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
            let items = input
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| AdapterError::Malformed("missing items".into()))?;
            items
                .iter()
                .map(|item| {
                    let reference = item
                        .as_str()
                        .ok_or_else(|| AdapterError::Malformed("item is not a string".into()))?;
                    Ok(Signal {
                        id: ulid::Ulid::new(),
                        source: Source::Webhook,
                        source_ref: reference.to_string(),
                        kind: SignalKind::Custom,
                        severity: Severity::Low,
                        title: format!("item {reference}"),
                        body: String::new(),
                        evidence: vec![],
                        fingerprint: merge0_signal::fingerprint(Source::Webhook, &[reference]),
                        join_keys: JoinKeys::default(),
                        affected_count: None,
                        delegated: false,
                        first_seen: chrono::DateTime::UNIX_EPOCH,
                        last_seen: chrono::DateTime::UNIX_EPOCH,
                        raw: item.clone(),
                    })
                })
                .collect()
        }
    }

    #[test]
    fn harness_accepts_conformant_output() {
        let fixture = r#"{"items": ["a"]}"#;
        let signals = FakeAdapter
            .normalize(&serde_json::from_str(fixture).unwrap())
            .unwrap();
        let mut expected = serde_json::to_value(&signals).unwrap();
        expected[0]["id"] = serde_json::Value::String(ULID_PLACEHOLDER.into());
        check_golden(&FakeAdapter, fixture, &expected.to_string());
    }

    #[test]
    #[should_panic(expected = "golden mismatch")]
    fn harness_rejects_divergent_output() {
        check_golden(&FakeAdapter, r#"{"items": ["a"]}"#, "[]");
    }
}
