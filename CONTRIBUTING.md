# Contributing to Merge0

Thanks for considering it. The fastest way to be useful here is to know
how the repo enforces its own quality bar — most of the review feedback
you'd normally get from humans is encoded in tests that run in CI.

## The four gates

All of these must pass before any commit (CI enforces them):

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # needs a local Postgres — see README (Development)
cd ui && npm test && npm run build
```

Read `CLAUDE.md` before non-trivial work: its architecture invariants
(adapter isolation, schema-changes-are-spec-changes, the MIT/`ee` boundary,
config-not-code for prompts) are enforced by tests in
`crates/merge0-e2e/tests/repo_hygiene.rs`, and a PR that fights them will
fail CI before it fights a reviewer.

## The most wanted contribution: adapters

Merge0's growth thesis is community adapters ("your tool → PRs"). An
adapter is a small, well-tested crate:

1. Copy the shape of an existing one (`crates/merge0-adapter-datadog` is a
   good minimal example; `merge0-adapter-posthog` a rich one).
2. Ground your payload shapes in vendor truth — link the vendor doc or
   source you derived them from in the module doc. Do not invent fields.
3. Golden fixtures through the shared harness in `crates/merge0-adapters`
   (bless mode: `MERGE0_BLESS=1`), using invented example.com data only.
4. Populate `join_keys` wherever derivable — cross-source correlation is
   the product.
5. Check the repo-hygiene rules: your goldens must include at least one
   signal at/above the shipped gate floor, and your source must be
   selectable by at least one shipped scout in `config/scouts/` (extend a
   scout's `sources` list in the same PR — the reachability test will tell
   you if you forgot).

## Fixing a bug? Encode the prevention

This repo's rule for its own development (CLAUDE.md invariant 9): a fix is
done when something *fails* if the bug comes back — a hygiene rule, a
regression test, an eval scenario. Verify your new test fails against the
reintroduced bug before trusting it; PRs that add a test which passes both
with and without the fix will be asked to prove the test can fail.

## Licensing of contributions

- Everything outside `ee/` is MIT. Everything inside `ee/` is under the
  Merge0 Enterprise License (`ee/LICENSE`).
- Contributions require agreeing to the [Contributor License
  Agreement](CLA.md) **before a PR can merge**. The CLA workflow comments
  on your first pull request with a one-line signature phrase; posting it
  records your signature (tied to your GitHub account) in the repo's
  signature ledger and turns the CLA check green. The CLA exists because
  Merge0 is open-core: the project needs the right to license contributed
  code on both sides of the `ee/` boundary. Contributing on behalf of an
  employer? Read the corporate-contributions section of `CLA.md` first.

## Evals cost money

The gate-eval corpus and agent fixtures run a real model and are **never
run in CI** — maintainers run them before releases. If your change touches
the gate prompt, context assembly, or memory, say so in the PR; the
maintainer will run the corpus and post before/after numbers (see
`evals/README.md` for the methodology, including why "still 100%" needs an
A/B to mean anything).
