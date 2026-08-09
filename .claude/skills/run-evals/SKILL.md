---
name: run-evals
description: Run the live-model evals (gate judgment corpus, agent fixtures) and update the baseline. Required whenever the gate prompt, memory rendering, or verdict parsing changes.
---

# Run the evals

The deterministic suite can't judge judgment: any change to the gate
prompt (`config/gate.toml`), memory/intent rendering, or model-output
parsing must be measured against the live corpus before shipping. Evals
cost real money and run manually, never in CI.

```sh
cargo run -p merge0-evals --bin gate-eval     # 30-scenario gate corpus
scripts/agent-eval.sh                          # agent fixtures (or one: scripts/agent-eval.sh districts)
```

Backend is the Claude Code CLI (`claude -p`), using your existing auth.
Optional: `MERGE0_EVAL_MODEL`, `MERGE0_EVAL_CLI`.

The discipline (see `evals/README.md` + `evals/BASELINE.md` for the full
history):

- **The bar is exit-code enforced**: gate accuracy ≥ 85% AND zero canary
  leaks. False WORK is worse than false SKIP.
- **Measure changes, don't assert them.** For a behavior change, A/B
  against the reconstructed pre-change behavior (see BASELINE.md run 6).
  For borderline scenarios, use `[expect] samples = N` +
  `min_pass_rate` — rates, not single points.
- **New fixture? Prove it red first.** Run the fixture project's tests
  before wiring it up — a fixture that starts green measures nothing
  (the utf8 fixture's truncation index initially landed on a char
  boundary and the "seeded" panic never fired).
- **Record the run in `evals/BASELINE.md`**: numbers, token cost, what
  changed, and honest caveats (e.g. the CLI-vs-`AnthropicModel` backend
  gap means this corpus cannot validate production sampling changes).
- **On a miss**, iterate the *prompt/config*, not the scorer — and if
  the miss revealed a new failure class, add a scenario so it stays
  covered (that's how the canary corpus grew).
