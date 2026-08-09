---
name: new-adapter
description: Add a new vendor signal source — scaffold the adapter crate, then walk the schema/fixture/reachability/fetch checklist. Use when asked to support a new analytics, monitoring, or ticketing tool.
---

# Add a vendor adapter

Run the scaffolder, then follow the checklist it prints:

```sh
scripts/new-adapter.sh <vendor>    # lowercase, e.g. "amplitude"
```

It generates `crates/merge0-adapter-<vendor>/` (Cargo.toml, `Adapter`
impl skeleton, golden test, fixture stub) and registers the crate in the
workspace. **The workspace then deliberately does not compile** until you
add the `Source` variant — a new source IS a schema change (invariant 2),
so `docs/signal-schema.md` bumps its version in the same PR.

The parts that are decisions, not boilerplate:

1. **Ground the payload shape in the vendor's real API** (docs or an
   actual response), then invent example.com fixture data (invariant 4 —
   never real payloads, never credentials). Bless expected files with
   `MERGE0_BLESS=1 cargo test -p merge0-adapter-<vendor>` and *review*
   the blessed output — blessing without reading is how wrong
   normalization gets pinned as correct.
2. **Document normalization decisions in the crate doc comment**
   (affected_count semantics, severity mapping, join_keys) — see
   `crates/merge0-adapter-openpanel/src/lib.rs` for the expected depth.
   Populate `join_keys` wherever derivable (invariant 3); severity maps
   conservative (a quiet inbox that's right beats a busy one).
3. **Make the source reachable by a shipped scout** in `config/scouts/`
   — the hygiene rule fails the build until every `(source, kind)` your
   goldens emit is selectable. This is the six-dead-sources bug
   (`docs/decision-log.md` #9); don't over-pin scout `sources` lists in
   tests, that's how a source ships unreachable.
4. **Fetch layer**: poller or webhook in `crates/merge0-fetch`
   (wiremock-tested; construction fails loudly when the source is
   enabled but unconfigured), then server ingest routing +
   `config/sources.toml` entry (secrets by env-var NAME only).
5. **Malformed rows are skipped, never envelope failures** — one corrupt
   row must not sink the page. And adapters never invent fields the
   schema doesn't have.

Finish with the `verify` skill (full, unscoped) and, if counts changed,
revisit e2e assertions — new sources change triage arithmetic.
