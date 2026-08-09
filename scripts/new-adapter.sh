#!/usr/bin/env bash
# Scaffold a new vendor adapter crate: crate skeleton, golden-test harness
# wiring, fixture stubs, and workspace registration — then print the
# checklist of the parts that are deliberately NOT generated (the schema
# variant, the scout reachability, the fetch wiring), because each of
# those is a reviewed decision, not boilerplate.
#
# Usage: scripts/new-adapter.sh <vendor>     (lowercase, e.g. "amplitude")
#
# The workspace will NOT compile until checklist step 1 (the Source
# variant + schema doc bump) is done — that is intentional: invariant 2
# says a new source IS a schema change.

set -euo pipefail
cd "$(dirname "$0")/.."

VENDOR="${1:?usage: scripts/new-adapter.sh <vendor> (lowercase alnum)}"
[[ "$VENDOR" =~ ^[a-z][a-z0-9]*$ ]] || {
  echo "error: vendor must be lowercase alphanumeric (got '$VENDOR')" >&2
  exit 2
}
CRATE="merge0-adapter-$VENDOR"
DIR="crates/$CRATE"
CAP="$(tr '[:lower:]' '[:upper:]' <<<"${VENDOR:0:1}")${VENDOR:1}"
SNAKE="merge0_adapter_$VENDOR"

[[ -e "$DIR" ]] && { echo "error: $DIR already exists" >&2; exit 1; }

mkdir -p "$DIR/src" "$DIR/tests/fixtures"

cat > "$DIR/Cargo.toml" <<EOF
[package]
name = "$CRATE"
description = "$CAP → Signals (TODO: which vendor payloads, one line)"
version.workspace = true
edition.workspace = true
license.workspace = true
publish.workspace = true

[dependencies]
merge0-signal.workspace = true
merge0-adapters.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
ulid.workspace = true
EOF

cat > "$DIR/src/lib.rs" <<EOF
//! $CAP → Signals.
//!
//! Supported envelope endpoints:
//!
//! - \`TODO_endpoint\` — TODO: which vendor API response this is.
//!
//! Normalization decisions (document them — see an existing adapter for
//! the expected depth; \`docs/decision-log.md\` #2 explains why):
//!
//! - **\`affected_count\`** is TODO (distinct humans, not event volume).
//! - **Severity** is TODO (conservative by design).
//! - **\`join_keys\`** are TODO (populate wherever derivable — invariant 3).

use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{Severity, Signal, SignalKind, Source};

pub struct ${CAP}Adapter;

impl Adapter for ${CAP}Adapter {
    fn source(&self) -> Source {
        // Compile error until you add the variant: a new source IS a
        // schema change (docs/signal-schema.md bumps in the same PR).
        Source::$CAP
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(e.to_string()))?;
        match envelope.endpoint.as_str() {
            "TODO_endpoint" => todo!("normalize the payload into Signals"),
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}
EOF

cat > "$DIR/tests/golden.rs" <<EOF
//! Golden-payload conformance tests: recorded $CAP payloads in, expected
//! Signals out. Regenerate expected files with \`MERGE0_BLESS=1 cargo test
//! -p $CRATE\` and review the diff.

use ${SNAKE}::${CAP}Adapter;
use merge0_adapters::testing::check_golden_files;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn typical() {
    check_golden_files(
        &${CAP}Adapter,
        &fixture("typical.json"),
        &fixture("typical.expected.json"),
    );
}
EOF

cat > "$DIR/tests/fixtures/typical.json" <<EOF
{
  "endpoint": "TODO_endpoint",
  "context": { "project_base_url": "https://$VENDOR.example.com/acme" },
  "payload": { "TODO": "a realistic vendor response, invented example.com data ONLY (invariant 4)" }
}
EOF

# --- register in the workspace ----------------------------------------
# after the last adapter entry in [workspace] members:
awk -v line="    \"crates/$CRATE\"," '
  /^[[:space:]]+"crates\/merge0-adapter-/ { last_member = NR }
  { lines[NR] = $0 }
  END { for (i = 1; i <= NR; i++) { print lines[i]; if (i == last_member) print line } }
' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
# after the last adapter entry in [workspace.dependencies]:
awk -v line="$CRATE = { path = \"crates/$CRATE\" }" '
  /^merge0-adapter-.* = \{ path/ { last_dep = NR }
  { lines[NR] = $0 }
  END { for (i = 1; i <= NR; i++) { print lines[i]; if (i == last_dep) print line } }
' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

cat <<EOF

SCAFFOLDED $DIR (registered in workspace members + dependencies).

The workspace will not compile yet — that is the checklist talking:

 1. Schema: add \`Source::$CAP\` in crates/merge0-signal/src/lib.rs
    (enum + as_str + FromStr) and bump docs/signal-schema.md's version
    with the new source IN THE SAME PR (invariant 2; the round-trip test
    keeps the doc's JSON example honest).
 2. Fixtures: replace tests/fixtures/typical.json with a realistic
    recorded-shape payload (invented example.com data only), then bless:
    MERGE0_BLESS=1 cargo test -p $CRATE — and REVIEW the blessed output.
    Add a minimal/malformed-row fixture too; malformed rows are skipped,
    never envelope failures.
 3. Reachability: make the new (source, kind) selectable by a shipped
    scout in config/scouts/ — the repo-hygiene rule
    every_adapter_source_is_selectable_by_at_least_one_shipped_scout
    fails the build until it is. An adapter no scout can reach is the
    six-dead-sources bug again (docs/decision-log.md #9).
 4. Fetch: wire a poller or webhook in crates/merge0-fetch (wiremock
    tests; fail loudly at construction if enabled-but-unconfigured) and
    ingest routing + config in crates/merge0-server + config/sources.toml
    (secrets by env-var NAME only).
 5. Docs: README source list + config reference.
 6. Verify: scripts/verify.sh (full, unscoped) before the PR.
EOF
