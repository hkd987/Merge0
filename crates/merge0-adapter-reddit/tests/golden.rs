//! Golden-payload conformance tests: recorded Reddit payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test
//! -p merge0-adapter-reddit` and review the diff.

use merge0_adapter_reddit::RedditAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn typical() {
    check_golden_files(
        &RedditAdapter,
        &fixture("typical.json"),
        &fixture("typical.expected.json"),
    );
}

#[test]
fn minimal() {
    check_golden_files(
        &RedditAdapter,
        &fixture("minimal.json"),
        &fixture("minimal.expected.json"),
    );
}
