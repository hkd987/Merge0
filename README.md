# Merge0

> **Status: Phase 0, pre-launch.** Private repo. The name "Merge0" is a
> placeholder. This repo goes public only after the Phase 0 merge-rate gate is
> met (see `docs/PRD.md`).

An open-source, insights-agnostic self-driving product loop: ingest signals
from the observability and support tools a team already uses, triage them into
evidence-backed work orders, and turn the actionable ones into reviewed,
test-passing pull requests — using the customer's own coding agent and compute.

```
┌───────────┐   ┌───────────────┐   ┌─────────┐   ┌──────────┐   ┌────────┐
│ Adapters  │──▶│ Context Store │──▶│ Triage  │──▶│  Runner  │──▶│ Inbox  │
│ (ingest)  │   │ (assembly)    │   │ (scouts │   │ (BYO     │   │ (+Slack│
│           │   │               │   │ + gate) │   │  agent)  │   │ digest)│
└───────────┘   └───────────────┘   └─────────┘   └──────────┘   └────────┘
                        ▲                                            │
                        └────────────── outcome memory ◀─────────────┘
```

## Workspace layout

| Path | What it is |
|---|---|
| `crates/merge0-signal` | The Signal schema — the contract between every component. See `docs/signal-schema.md`. |
| `crates/merge0-adapters` | `Adapter` trait + golden-payload conformance harness. |
| `crates/merge0-adapter-posthog` | PostHog → Signals (error tracking issues, dead/rage-click sessions). |
| `crates/merge0-adapter-sentry` | Sentry → Signals (issues, releases). |
| `crates/merge0-context` | Context store: intent / correlation / release / outcome memory. |
| `crates/merge0-triage` | Scouts + clustering + gate. Scout/gate prompts are config files in `config/`, not code. |
| `crates/merge0-runner` | BYO-agent, BYO-compute dispatch (`WorkOrder in → PRResult out`). |
| `crates/merge0-server` | Axum service + inbox (stub). |
| `config/` | Scout and gate configuration — versioned here so the future meta-loop can propose changes as ordinary PRs. |
| `docs/` | PRD and the versioned Signal schema spec. |
| `ee/` | Commercial (non-MIT) directory — empty placeholder until Phase 2. |

## Build & test

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
```

## Architecture invariants

- **Adapter isolation:** the core never imports vendor types. Adapters emit
  normalized Signals; conformance is golden-payload tested.
- **BYO compute:** customer code never transits Merge0 servers.
- **No auto-merge, ever.** A human clicks merge.
- **Self-improvement lands in artifacts, never the executor** (PRD §5d):
  learned changes go to git-visible, human-merged files — configs, tests,
  rules, fenced doc sections.

## License

MIT (see `LICENSE`), except the `ee/` directory which is under a commercial
license (see `ee/LICENSE`).
