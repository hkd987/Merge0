//! Golden-payload conformance tests: recorded Mixpanel payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-mixpanel` and review the diff.

use merge0_adapter_mixpanel::MixpanelAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn funnels_typical() {
    check_golden_files(
        &MixpanelAdapter,
        &fixture("funnels_typical.json"),
        &fixture("funnels_typical.expected.json"),
    );
}

#[test]
fn funnels_minimal() {
    check_golden_files(
        &MixpanelAdapter,
        &fixture("funnels_minimal.json"),
        &fixture("funnels_minimal.expected.json"),
    );
}
