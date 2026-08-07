//! Golden-payload conformance tests: recorded webhook/OTLP payloads in,
//! expected Signals out. Regenerate expected files with `MERGE0_BLESS=1
//! cargo test -p merge0-adapter-webhook` and review the diff.

use merge0_adapter_webhook::{OtelAdapter, WebhookAdapter};
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn signals_typical() {
    check_golden_files(
        &WebhookAdapter,
        &fixture("signals_typical.json"),
        &fixture("signals_typical.expected.json"),
    );
}

#[test]
fn signals_minimal() {
    check_golden_files(
        &WebhookAdapter,
        &fixture("signals_minimal.json"),
        &fixture("signals_minimal.expected.json"),
    );
}

#[test]
fn otel_logs_typical() {
    check_golden_files(
        &OtelAdapter,
        &fixture("otel_logs_typical.json"),
        &fixture("otel_logs_typical.expected.json"),
    );
}

#[test]
fn otel_logs_minimal() {
    check_golden_files(
        &OtelAdapter,
        &fixture("otel_logs_minimal.json"),
        &fixture("otel_logs_minimal.expected.json"),
    );
}
