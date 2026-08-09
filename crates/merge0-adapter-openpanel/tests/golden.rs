//! Golden-payload conformance tests: recorded OpenPanel payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-openpanel` and review the diff.

use merge0_adapter_openpanel::OpenpanelAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn events_typical() {
    check_golden_files(
        &OpenpanelAdapter,
        &fixture("events_typical.json"),
        &fixture("events_typical.expected.json"),
    );
}

#[test]
fn events_minimal() {
    check_golden_files(
        &OpenpanelAdapter,
        &fixture("events_minimal.json"),
        &fixture("events_minimal.expected.json"),
    );
}
