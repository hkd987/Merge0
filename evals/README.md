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

Scenarios live in `evals/scenarios/*.toml` (30 cases). The first 14 are
hand-built controls: clear crashes, cross-source corroboration, release
regressions, intended-behavior traps, vague noise, deterministic-guard
controls, a secret-value canary, repeated reverts, prompt-injection, a
feature request. Scenarios 15–27 model the shapes of real high-traffic
GitHub issues (anonymized to the example.com domain): flaky tests, perf
regressions, memory leaks, docs drift, CVEs, an XSS report with a payload
canary, "works on my machine" noise, rewrite demands, support questions,
by-design closures, encoding corruption, and a user-bisected regression.
Scenarios 28–30 cover memory: a constraint that lives only in the
machine-managed fence at the end of a long intent doc (must SKIP); a solid
defect carrying two reverts from over two years ago (must still WORK — aged
memory informs, it does not veto); and its twin where those reverts carry
the PR they produced, which the Work Order must then cite. Scoring is
deterministic:
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

Fixtures in `evals/fixtures/*/`. Five expect **fix** (tests green, diff
within budget, tests untouched): a null-handling crash, an off-by-one, a
silently swallowed `Err(_)`, a byte-index slice that panics mid-character
on non-ASCII text, and a cache that is never invalidated. One expects
**discard** — a work order that contradicts the repo's policy-encoding
tests, where the agent must not weaken the tests or ship a
policy-violating green diff.

Adding a fixture: seed the defect, write the tests that encode the success
criteria, and **run `cargo test` in the fixture project before wiring it
up** — a fixture that starts green measures nothing, and the harness's
baseline-sanity check is there to catch exactly that. (The utf8 fixture
started green on its first draft because the truncation index happened to
land on a character boundary in the strings chosen.)

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

## Proving a change improved something

"30/30 after" is not evidence when the corpus was already at 100% before.
When a change alters what the gate *sees* (context assembly, memory,
prompt), measure it as an A/B: reconstruct the old behavior in the working
tree, run the full corpus both ways on the same day and backend, and record
both numbers — run 6 in `BASELINE.md` is the worked example.

Two habits that keep the result honest:

- **A new scenario is only a canary once it fails against the old
  behavior.** One that passes both ways measures nothing, however good the
  prose in its `description` is. Check before trusting it, exactly as
  `crates/merge0-e2e/tests/repo_hygiene.rs` requires of a new lint rule.
- **Repeat borderline scenarios and report the rate, not one run.** Model
  judgment is not deterministic; a single flip can be noise. A scenario
  declares this itself:

  ```toml
  [expect]
  decision = "work"
  samples = 5          # default 1
  min_pass_rate = 0.8  # default 1.0 — decisive scenarios stay strict
  ```

  Use it only where the corpus documents *why* the case is a judgment call,
  and say so in `BASELINE.md`. Loosening a bar to quiet a scenario that
  should be decisive is how a corpus becomes decoration. Canary checks are
  unaffected: a leak in any single sample counts as a leak, whatever the
  pass rate.
