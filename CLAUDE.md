# CLAUDE.md — Merge0

Merge0 is an insights-agnostic self-driving product loop: adapters normalize
vendor signals (PostHog, Sentry, …) into a common Signal schema, triage turns
them into evidence-backed Work Orders, and a BYO-agent runner turns approved
Work Orders into test-passing PRs. Read `docs/PRD.md` before non-trivial work;
`docs/signal-schema.md` is the normative schema spec.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # CI-enforced
```

All three of fmt/clippy/test must pass before any commit.

## Architecture invariants (uphold in every change)

1. **Adapter isolation.** `merge0-signal`, `merge0-context`, `merge0-triage`,
   `merge0-runner`, and `merge0-server` never import vendor types or depend on
   `merge0-adapter-*` crates. Adapters depend on `merge0-signal` +
   `merge0-adapters` only.
2. **Schema changes are spec changes.** Any change to the types in
   `merge0-signal` must update `docs/signal-schema.md` (and its version) in the
   same PR. A unit test round-trips the doc's JSON example — keep them in sync.
   Adapters may not invent fields.
3. **Golden-payload conformance.** Every adapter behavior change needs a
   fixture + expected-Signal pair run through the shared harness in
   `merge0-adapters`. Populate `join_keys` wherever derivable.
4. **No credentials in code, fixtures, or tests.** Secret references are by
   env-var *name* only. Fixtures use invented example.com data.
5. **MIT/`ee` boundary.** Nothing under `crates/` may depend on `ee/`.
6. **Scout/gate prompts are config, not code** — they live in `config/` so the
   meta-loop can later propose changes as ordinary PRs.
7. **Well tested is the bar.** New logic ships with tests: golden tests for
   normalization, unit tests for mappings/edge cases. Untested code is
   incomplete code.

## Self-improvement protocol (standing instruction)

This file learns. Whenever a mistake is caught during a session — a failing
test you had to fix, a clippy/fmt error, a wrong assumption about a vendor
payload or crate API, review feedback, or a user correction — before ending
the turn, append a one-line generalized lesson to the fenced section below so
future sessions don't repeat it.

Rules (mirroring PRD §5c/§5d discipline):

- Write **inside the fence only**; never rewrite human-authored prose outside
  it. This is the same fence rule Merge0 itself enforces for intent docs.
- One line per lesson: `- [YYYY-MM-DD] <generalized lesson>` — state the rule,
  not the anecdote.
- Deduplicate before appending; prune entries that have become stale or have
  been promoted into a better artifact (a lint rule, a test, a doc section).
  Prefer promoting recurring lessons into enforced artifacts over letting the
  list grow — an ever-growing list is context rot.
- Lessons are for *generalizable* mistakes. Don't log one-off typos.

<!-- merge0:lessons:start (machine-managed — appended by Claude; pruned periodically; do not hand-edit outside PR review) -->
- [2026-08-07] Deps used only inside `#[cfg(test)]` still need a `[dev-dependencies]` entry in that crate's own Cargo.toml — being in `[workspace.dependencies]` is not enough; `cargo build` succeeding does not prove `cargo test` compiles.
<!-- merge0:lessons:end -->
