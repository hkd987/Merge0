//! Golden-payload conformance tests: recorded Asana payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-asana` and review the diff.

use merge0_adapter_asana::AsanaAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn tasks_typical() {
    check_golden_files(
        &AsanaAdapter,
        &fixture("tasks_typical.json"),
        &fixture("tasks_typical.expected.json"),
    );
}

#[test]
fn tasks_minimal() {
    check_golden_files(
        &AsanaAdapter,
        &fixture("tasks_minimal.json"),
        &fixture("tasks_minimal.expected.json"),
    );
}
