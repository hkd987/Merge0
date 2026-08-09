//! Golden-payload conformance tests: recorded X payloads in, expected
//! Signals out. Regenerate expected files with `MERGE0_BLESS=1 cargo test
//! -p merge0-adapter-x` and review the diff.

use merge0_adapter_x::XAdapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Two posts: a viral complaint (engagement 296 → high, above the shipped
/// gate floor) and a low-engagement mention; non-ASCII text exercises the
/// char-boundary title truncation.
#[test]
fn typical() {
    check_golden_files(
        &XAdapter,
        &fixture("typical.json"),
        &fixture("typical.expected.json"),
    );
}

/// A malformed row (no `text`) is skipped, and the surviving post's author
/// has no `includes.users` row — the `author_id` fallback path.
#[test]
fn minimal() {
    check_golden_files(
        &XAdapter,
        &fixture("minimal.json"),
        &fixture("minimal.expected.json"),
    );
}

/// Zero results: X omits `data` entirely — zero signals, not an error.
#[test]
fn empty() {
    check_golden_files(
        &XAdapter,
        &fixture("empty.json"),
        &fixture("empty.expected.json"),
    );
}
