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
8. **All UI follows `docs/style-guide.md`.** Colors, type, spacing, and radii
   come only from the tokens in `ui/src/theme.css` (enforced by the style-lint
   test); the inbox stays a keyboard-first review queue, pages stay data-free
   static assets with client-side token auth. Change the system via tokens +
   the guide in the same PR, never by special-casing a component.
9. **Fixing a bug means encoding the prevention.** Apply Merge0's own
   hardening hierarchy to Merge0 (`merge0-hardening`: LintRule >
   RegressionTest > IntentAmendment). Before calling a fix done, ask which
   artifact stops it recurring — in order of preference: a rule in
   `crates/merge0-e2e/tests/repo_hygiene.rs` or a clippy lint (fails the
   build for everyone, forever), a regression test (when the pattern is
   behavioral), an eval canary (when only a live model exposes it), and only
   as a last resort a CLAUDE.md lesson (prose has never failed a build).
   A rule must be a real incident and precisely detectable: false positives
   train people to ignore the lint, which is worse than not having it.
   Verify a new rule *fails* on the reintroduced bug before trusting it.

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
- [2026-08-07] Compute fixture constants (epoch nanos, hashes) programmatically or via bless mode, never by mental arithmetic.
- [2026-08-07] Postgres `timestamptz` truncates to microseconds: a chrono timestamp written and read back can compare `<` its original. Never use exact `>=`-on-now filters against round-tripped timestamps; add a small tolerance or truncate before storing.
- [2026-08-07] In axum handlers, extractor-based body parsing (`Json<T>`) runs before the handler body, so auth checks inside the handler happen after a 422 parse rejection. For authenticated endpoints, take `Bytes` and parse after the auth check.
- [2026-08-07] clippy's `await_holding_lock` is not satisfied by an explicit `drop(guard)` — scope the `MutexGuard` in a block that ends before the `.await`.
- [2026-08-07] "Local clippy clean" proves nothing if CI resolves a newer stable with new lints — pin the toolchain version in rust-toolchain.toml (not `channel = "stable"`) and verify with the pinned version before pushing.
- [2026-08-07] Clean up after local verification (user instruction): `cargo clean`, drop throwaway DB schemas, stop the dev Postgres when done — a 13GB target/ plus idle daemons caused disk pressure and an OOM-killed Postgres mid-test-run.
- [2026-08-07] `Result::unwrap_err()`/`expect_err()` need `Debug` on the Ok type — for non-Debug Ok sides (trait objects) use `.err().expect(...)`; when the Ok type IS Debug, clippy's `err_expect` lint demands `expect_err` instead.
- [2026-08-07] Official `rust:` Docker images ship the minimal rustup profile, so a rust-toolchain.toml that requests components forces a mid-build channel download — set `RUSTUP_TOOLCHAIN` in the builder stage to use the preinstalled toolchain.
- [2026-08-07] File-sized build inputs (CA bundles, keys) blow past `--build-arg` argv limits — pass them into `docker build` as BuildKit secret mounts.
- [2026-08-07] Model-output parsers must tolerate benign shape variance (models emit lists where a string was asked for): strict serde + fail-closed silently zeroes yield — a failure class only live-model evals catch, never scripted-model tests.
- [2026-08-07] In diff-measuring harnesses, build side-products (Cargo.lock, target/) must be in the base commit or .gitignore, or the measurement blames the agent for them.
- [2026-08-08] New sources change triage arithmetic: e2e assertions on report/work-order counts must be revisited whenever a scout's source list grows, and report lookups should select by content (title match), never by index.
- [2026-08-08] E2e payloads need generated now-relative timestamps, not values copied from fixtures — scouts filter on `last_seen >= period_start`, so a stale epoch silently drops the signal from triage; and work-order expectations must respect the shipped gate's max_work_orders_per_run cap.
- [2026-08-08] The working tree may carry another session's in-flight change (e.g. a new required Signal field): write new code against the tree's current structs, not the last commit or stale reads, and attribute workspace-wide build breaks to the right diff before "fixing" them.
- [2026-08-08] The gate's secret-redaction discipline must extend to security-report content: working exploit payloads and attacker exfil endpoints leak into Work Orders (and thence PR bodies/Slack) exactly like credentials — forbid the verbatim payload + endpoint in eval canaries, but not ubiquitous API identifiers (e.g. `document.cookie`), which the model needs to write a useful repro.
- [2026-08-08] Workspace-wide tooling (`cargo fmt --all`, `clippy --fix`) writes to crates outside your task's scope — when another session has in-flight work in the tree, scope verification to `-p <crate>` and only run the `--all` form once the tree is yours.
- [2026-08-08] PROMOTED (now CI-enforced in `crates/merge0-e2e/tests/repo_hygiene.rs`, so they are deleted from this list rather than restated): SQL `SUM()` must be cast `::BIGINT`; raw `auth_header` values keep the `Bearer ` prefix; every adapter must emit at least one signal at/above the shipped gate floor. Prefer adding a rule there over adding a line here.
- [2026-08-09] `cargo fmt --all --check` REPORTS diffs, it never applies them — and a `&& echo OK` gate is worthless unless you confirm the OK actually printed. Never claim a gate passed without seeing its positive marker; absence of an error line is not a pass.
- [2026-08-08] `git checkout -- <file>` discards UNCOMMITTED work — never use it to undo a temporary edit while a subagent's changes to that file are still unstaged; snapshot with `cp` and restore from the copy. Related: a mutation test that greps for a string the code does not actually contain silently passes — assert the mutation changed the file before trusting the result.
<!-- merge0:lessons:end -->
