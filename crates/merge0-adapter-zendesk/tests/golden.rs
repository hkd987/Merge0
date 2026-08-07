//! Golden-payload conformance tests: recorded Zendesk payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-zendesk` and review the diff.

use merge0_adapter_zendesk::ZendeskAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn tickets_typical() {
    check_golden_files(
        &ZendeskAdapter,
        &fixture("tickets_typical.json"),
        &fixture("tickets_typical.expected.json"),
    );
}

#[test]
fn tickets_minimal() {
    check_golden_files(
        &ZendeskAdapter,
        &fixture("tickets_minimal.json"),
        &fixture("tickets_minimal.expected.json"),
    );
}
