# Merge0 evals — do the model judgments earn merged PRs?

Everything deterministic in Merge0 is covered by the workspace test suite.
These evals cover the two judgments that decide product quality with a
**real model**, and they cost **real money** — run them manually, never in
CI. The model backend is the Claude Code CLI (`claude -p`), using whatever
auth your CLI already holds (same BYO posture as the runner).

## 1. Gate judgment (`gate-eval`)

Does the gate — real gate code, the SHIPPED `config/gate.toml` prompt —
make the right work/skip calls on a curated corpus?

```sh
cargo run -p merge0-evals --bin gate-eval
# optional: MERGE0_EVAL_MODEL=<model id>   MERGE0_EVAL_CLI=<path to claude>
```

Scenarios live in `evals/scenarios/*.toml` (27 cases). The first 14 are
hand-built controls: clear crashes, cross-source corroboration, release
regressions, intended-behavior traps, vague noise, deterministic-guard
controls, a secret-value canary, repeated reverts, prompt-injection, a
feature request. Scenarios 15–27 model the shapes of real high-traffic
GitHub issues (anonymized to the example.com domain): flaky tests, perf
regressions, memory leaks, docs drift, CVEs, an XSS report with a payload
canary, "works on my machine" noise, rewrite demands, support questions,
by-design closures, encoding corruption, and a user-bisected regression.
Scoring is deterministic:
decision correctness, zero-token proof for guard cases, content mentions,
canary absence. Results land in `evals/results/` (gitignored); the first
measured run is recorded in `BASELINE.md`.

**The bar** (exit code enforces it): decision accuracy ≥ 85% AND zero
secret-canary leaks. Directionally, false WORK (emitting work the corpus
says to skip) burns reviewer trust and matters more than false SKIP.

## 2. Agent run (`agent-eval.sh`)

Does a Work Order become a small, test-passing diff? Mirrors the generated
Actions workflow exactly: sanitized work-order JSON as the whole prompt,
the same `--allowedTools` discipline, the same repair loop and git-based
diff-budget measurement.

```sh
scripts/agent-eval.sh              # all fixtures
scripts/agent-eval.sh districts    # one fixture
```

Fixtures in `evals/fixtures/*/`: a seeded null-handling crash and an
off-by-one (expected outcome: **fix** — tests green, diff within budget,
tests untouched), and a work order that contradicts the repo's
policy-encoding tests (expected outcome: **discard** — the agent must not
weaken the tests or ship a policy-violating green diff).

## Improving on a miss

A failing eval is the meta-loop (PRD §5d) in manual form: tune
`config/gate.toml`'s prompt (or the workflow/agent config), rerun, and
commit the config change with the before/after numbers in the PR — the
eval output is the evidence. Never special-case a scenario in code to make
it pass; the corpus is the spec.

Adding a scenario: copy any file in `evals/scenarios/`, keep expectations
checkable (`decision`, `work_order_mentions`, `forbidden`), and run
`cargo test -p merge0-evals` — the corpus is loaded and validated by unit
tests, so a malformed scenario fails CI deterministically.
