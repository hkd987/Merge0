//! Golden-payload conformance tests (PRD P0-1): recorded PostHog payloads in,
//! expected Signals out. Regenerate expected files with `MERGE0_BLESS=1
//! cargo test -p merge0-adapter-posthog` and review the diff.

use merge0_adapter_posthog::PosthogAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn error_tracking_issues_typical() {
    check_golden_files(
        &PosthogAdapter,
        &fixture("error_tracking_issues_typical.json"),
        &fixture("error_tracking_issues_typical.expected.json"),
    );
}

#[test]
fn error_tracking_issues_minimal() {
    check_golden_files(
        &PosthogAdapter,
        &fixture("error_tracking_issues_minimal.json"),
        &fixture("error_tracking_issues_minimal.expected.json"),
    );
}

#[test]
fn rageclick_events() {
    check_golden_files(
        &PosthogAdapter,
        &fixture("rageclick_events.json"),
        &fixture("rageclick_events.expected.json"),
    );
}
