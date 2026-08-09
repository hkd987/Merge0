//! Golden-payload conformance tests: recorded Trello payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test -p
//! merge0-adapter-trello` and review the diff.

use merge0_adapter_trello::TrelloAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn cards_typical() {
    check_golden_files(
        &TrelloAdapter,
        &fixture("cards_typical.json"),
        &fixture("cards_typical.expected.json"),
    );
}

#[test]
fn cards_minimal() {
    check_golden_files(
        &TrelloAdapter,
        &fixture("cards_minimal.json"),
        &fixture("cards_minimal.expected.json"),
    );
}
