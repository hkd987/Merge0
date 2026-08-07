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
| `crates/merge0-signal` | The Signal schema (v0.2) + pipeline contract: Reports, Work Orders (with diff budgets), gate decisions, outcomes, telemetry math. Spec: `docs/signal-schema.md`. |
| `crates/merge0-adapters` | `Adapter` trait + golden-payload conformance harness (`MERGE0_BLESS=1` to regenerate goldens). |
| `crates/merge0-adapter-{posthog,sentry}` | Phase 0 adapters (P0-1/P0-2). |
| `crates/merge0-adapter-{zendesk,github-issues,webhook,datadog,loopforge}` | P1/P2 adapters. `webhook` also carries the OTLP-logs adapter; its `signals` endpoint is the published integration surface. |
| `crates/merge0-store` | Postgres, schema-per-tenant: deduped signals, report lifecycle, outcome memory (incl. revert mapping), releases, telemetry queries, versioned migrations, webhook-delivery dedupe, fetch cursors. |
| `crates/merge0-fetch` | The vendor I/O layer: pollers (PostHog, Sentry, Zendesk, Datadog, GitHub Issues) driven by `config/sources.toml` + native webhook verification/envelope builders. Feeds the adapters in-process. |
| `crates/merge0-model` | Model abstraction: Anthropic client (BYO key) + scripted fake. |
| `crates/merge0-context` | Intent docs (MERGE0.md fence enforcement), release attribution, prior-attempts assembly. |
| `crates/merge0-triage` | Scouts (config-driven) → deterministic clustering → model gate. Evidence budgets, Opportunity Reports, fail-closed gate parsing. |
| `crates/merge0-github` | GitHub App auth (installation tokens only), API trait + fake, webhooks + revert detection, safety verification. |
| `crates/merge0-runner` | `repository_dispatch` runner, sanitized payloads, budget enforcement, agent-agnostic configs, the customer-side Actions workflow template. |
| `crates/merge0-hardening` | Post-merge prevention PRs: lint > regression test > fenced intent-doc amendment (PRD §5c). |
| `crates/merge0-meta` | The meta-loop (PRD §5d): telemetry as a Signal source + meta-scout config-change PRs. |
| `crates/merge0-slack` | Block Kit messages, interaction parsing, signature verification, weekly digest. |
| `crates/merge0-broker` | P2 credential broker: per-Work-Order, single-repo, ≤10-minute tokens via git credential helper. |
| `crates/merge0-registry` | P2 curated skill registry: ed25519-signed index, installs as manifest-change PRs. |
| `crates/merge0-server` | The Axum service: ingestion (envelope + native vendor webhooks), scheduled fetch + triage, inbox, onboarding bundle, runner callback, GitHub webhooks, hardening + meta triggers, telemetry dashboard, Slack digest + interactions. |
| `crates/merge0-e2e` | Full-pipeline end-to-end tests. |
| `ee/merge0-ee` | Commercial (non-MIT): multi-tenant org management, RBAC, audit log, metering/billing, cross-tenant outcome priors. |
| `ee/merge0-hosted` | Commercial control-plane binary over `merge0-ee`: tenant lifecycle, membership, usage/invoice, audit, priors — behind `MERGE0_EE_ADMIN_TOKEN`. |
| `config/` | Scout + gate prompts and budgets — versioned config so the meta-loop proposes changes as ordinary PRs. |

## Build & test

Tests for the store, triage, server, e2e, and ee crates need Postgres:

```sh
# one-time local cluster (or point MERGE0_TEST_DATABASE_URL anywhere)
initdb -D .pg -U merge0 && pg_ctl -D .pg -o "-p 55432" start
createdb -h localhost -p 55432 -U merge0 merge0

cargo build --workspace
cargo test --workspace
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
```

CI runs the same three gates against a Postgres 16 service container.

## Running the server

```sh
MERGE0_DATABASE_URL=postgres://merge0@localhost:55432/merge0 \
MERGE0_REPO=your-org/your-repo \
ANTHROPIC_API_KEY=... \
MERGE0_GITHUB_APP_ID=... \
MERGE0_GITHUB_APP_PRIVATE_KEY=... \
MERGE0_GITHUB_INSTALLATION_ID=... \
MERGE0_API_TOKEN=$(openssl rand -hex 24) \
MERGE0_RUNNER_TOKEN=$(openssl rand -hex 24) \
MERGE0_GITHUB_WEBHOOK_SECRET=... \
cargo run -p merge0-server
```

Optional env:

- `MERGE0_SLACK_WEBHOOK_URL` + `MERGE0_SLACK_SIGNING_SECRET` — digest and
  interactive Approve/Dismiss from Slack.
- Vendor webhook verification (only for `/webhooks/{vendor}` you point at the
  server): `MERGE0_SENTRY_WEBHOOK_SECRET`, `MERGE0_POSTHOG_WEBHOOK_TOKEN`,
  `MERGE0_ZENDESK_WEBHOOK_SECRET`, `MERGE0_DATADOG_WEBHOOK_TOKEN`.
- `MERGE0_TRIAGE_INTERVAL_SECS` (default nightly; the fetch layer polls the
  sources enabled in `config/sources.toml` on the same schedule),
  `MERGE0_HARDENING_ENABLED=1` (§5c prevention PRs after merges),
  `MERGE0_META_ENABLED=1` (weekly §5d config-change proposals),
  `MERGE0_RAW_RETENTION_DAYS` (purge verbatim vendor payloads after N days).
- `MERGE0_AGENT` (`codex-cli` or `custom:<command>`), `MERGE0_GATE_MODEL`,
  `MERGE0_INTENT_FALLBACK` (used until `MERGE0.md` exists in the repo —
  the intent doc is fetched from the customer repo on every triage run).

Surfaces: `/inbox` (review queue; paste the API token once, stored in the
browser), `/onboarding` (generated workflow + MERGE0.md + agent.toml bundle
and setup checklist), `/telemetry`, `/safety`, `POST /ingest/{source}`
(envelope), `POST /webhooks/{sentry,posthog,zendesk,datadog}` (native,
vendor-signature-verified), `POST /triage/run`, `POST /slack/interactions`.
All product routes require `Authorization: Bearer $MERGE0_API_TOKEN`;
webhooks and the runner callback authenticate with their own schemes.

## Deploying

```sh
cp .env.example .env   # fill in
docker compose up --build
```

The `Dockerfile` is a multi-stage build pinned to the same toolchain as CI
(building behind a TLS-inspecting proxy: pass its CA with
`docker build --secret id=extra_ca_certs,src=proxy-ca.pem .`). On Coolify
(PRD target: a small DO box), create a Docker Compose service from this
repo, set the `.env` values in the UI, and put the app behind HTTPS —
`MERGE0_PUBLIC_URL` must match the public URL GitHub/vendors call back to.
The hosted control plane (`merge0-hosted`, in the image) is a separate
binary — run it only for multi-tenant installs.

For customer-repo setup, `GET /onboarding` returns the three files to commit
(`.github/workflows/merge0.yml`, `MERGE0.md`, `.merge0/agent.toml`) plus the
Actions secrets to configure.

## Architecture invariants

- **Adapter isolation:** the core never imports vendor types. Adapters emit
  normalized Signals; conformance is golden-payload tested.
- **BYO compute:** customer code never transits Merge0 servers; the runner
  workflow executes in the customer's CI with their keys.
- **No auto-merge, ever.** A human clicks merge.
- **No red PRs:** runs self-repair within budget, then self-discard — with
  the diagnosis salvaged onto the report.
- **Credentials:** GitHub App installation tokens only; secrets referenced by
  name and resolved customer-side; every secret type redacts its Debug.
- **Self-improvement lands in artifacts, never the executor** (PRD §5d):
  hardening and meta-loop changes arrive as evidence-linked, human-merged PRs.

## License

MIT (see `LICENSE`), except the `ee/` directory which is under a commercial
license (see `ee/LICENSE`).
