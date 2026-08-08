//! Golden-payload conformance tests: recorded Jira payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-jira` and review the diff.

use merge0_adapter_jira::JiraAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn issues_typical() {
    check_golden_files(
        &JiraAdapter,
        &fixture("issues_typical.json"),
        &fixture("issues_typical.expected.json"),
    );
}

#[test]
fn issues_minimal() {
    check_golden_files(
        &JiraAdapter,
        &fixture("issues_minimal.json"),
        &fixture("issues_minimal.expected.json"),
    );
}
