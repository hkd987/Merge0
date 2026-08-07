//! Golden-payload conformance tests: recorded Datadog payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-datadog` and review the diff.

use merge0_adapter_datadog::DatadogAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn events_typical() {
    check_golden_files(
        &DatadogAdapter,
        &fixture("events_typical.json"),
        &fixture("events_typical.expected.json"),
    );
}

#[test]
fn events_minimal() {
    check_golden_files(
        &DatadogAdapter,
        &fixture("events_minimal.json"),
        &fixture("events_minimal.expected.json"),
    );
}
