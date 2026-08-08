//! Golden-payload conformance tests: recorded Slack payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-slack` and review the diff.

use merge0_adapter_slack::SlackAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn messages_typical() {
    check_golden_files(
        &SlackAdapter,
        &fixture("messages_typical.json"),
        &fixture("messages_typical.expected.json"),
    );
}

#[test]
fn messages_minimal() {
    check_golden_files(
        &SlackAdapter,
        &fixture("messages_minimal.json"),
        &fixture("messages_minimal.expected.json"),
    );
}
