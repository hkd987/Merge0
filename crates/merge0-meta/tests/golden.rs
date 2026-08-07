//! Golden-payload conformance tests: recorded telemetry envelopes in,
//! expected Signals out. Regenerate expected files with `MERGE0_BLESS=1
//! cargo test -p merge0-meta` and review the diff.

use merge0_adapters::testing::check_golden_files;
use merge0_meta::MetaAdapter;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn telemetry_unhealthy() {
    check_golden_files(
        &MetaAdapter,
        &fixture("telemetry_unhealthy.json"),
        &fixture("telemetry_unhealthy.expected.json"),
    );
}

#[test]
fn telemetry_healthy() {
    check_golden_files(
        &MetaAdapter,
        &fixture("telemetry_healthy.json"),
        &fixture("telemetry_healthy.expected.json"),
    );
}
