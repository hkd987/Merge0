//! Golden-payload conformance tests: recorded Loopforge payloads in,
//! expected Signals out. Regenerate expected files with `MERGE0_BLESS=1
//! cargo test -p merge0-adapter-loopforge` and review the diff.

use merge0_adapter_loopforge::LoopforgeAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn review_findings_typical() {
    check_golden_files(
        &LoopforgeAdapter,
        &fixture("review_findings_typical.json"),
        &fixture("review_findings_typical.expected.json"),
    );
}

#[test]
fn review_findings_minimal() {
    check_golden_files(
        &LoopforgeAdapter,
        &fixture("review_findings_minimal.json"),
        &fixture("review_findings_minimal.expected.json"),
    );
}
