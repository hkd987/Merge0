//! Golden-payload conformance tests: recorded Intercom payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-intercom` and review the diff.

use merge0_adapter_intercom::IntercomAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn conversations_typical() {
    check_golden_files(
        &IntercomAdapter,
        &fixture("conversations_typical.json"),
        &fixture("conversations_typical.expected.json"),
    );
}

#[test]
fn conversations_minimal() {
    check_golden_files(
        &IntercomAdapter,
        &fixture("conversations_minimal.json"),
        &fixture("conversations_minimal.expected.json"),
    );
}
