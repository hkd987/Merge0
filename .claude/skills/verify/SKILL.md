---
name: verify
description: Run the repo's merge gates (fmt, clippy, tests, ui) in CI order with positive-marker discipline. Use before any commit, and always unscoped before shipping.
---

# Verify the tree

Run `scripts/verify.sh`. It executes the four merge gates in CI order and
prints an explicit `GATE <name>: OK` marker per gate — a gate has passed
only if its marker printed. Never claim green from absent error output
(`docs/decision-log.md` #12: `cargo fmt --all --check` *reports* diffs, it
never applies them).

```sh
scripts/verify.sh                 # full run: fmt, clippy, test, ui
scripts/verify.sh -p merge0-store # scope clippy+test to one crate
scripts/verify.sh --check         # CI parity (fmt reports instead of rewriting)
```

Rules:

- **Tests need Postgres** at `postgres://merge0@localhost:55432/merge0`
  (or `MERGE0_TEST_DATABASE_URL`). Start it with `scripts/dev-pg.sh start`
  (see the `dev-postgres` skill). The script checks reachability before
  burning a compile cycle.
- **Scope with `-p` while others have in-flight work in the tree** —
  workspace-wide `fmt --all`/`clippy --fix` rewrite crates outside your
  task. Before shipping, run the full unscoped form; the script's summary
  line distinguishes `ALL GATES GREEN (full workspace)` from
  `PARTIAL RUN`. Only the former is shippable.
- Toolchain is pinned in `rust-toolchain.toml` — do not verify with a
  different stable and assume CI agrees.
- After a heavy verification cycle, clean up: `cargo clean`, stop the dev
  Postgres (`scripts/dev-pg.sh stop`), remove `ui/node_modules` if you
  installed it just for this.
