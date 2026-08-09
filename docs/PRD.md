# Merge0 — Product Requirements Document

**Status:** v1.1 — Ready for engineering review
**Last updated:** August 6, 2026 (v1.1: self-improvement discipline §5d, meta-loop, Opportunity Reports, diff budget, failed-run salvage, external merge-rate calibration)
**Owner:** AdminRemix LLC (founder)
**Entity:** AdminRemix LLC (standalone venture)
**Design Partner Zero:** "Chalk" — the internal codename for the first design-partner SaaS (deliberately the same invented name used in this repo's test fixtures and example data)
**Distribution:** Open-core — MIT core + `/ee` commercial directory, with a hosted version (see Distribution & Open Source Strategy)
**Working name:** "Merge0" is a placeholder — see Open Questions.

---

## One-liner

An open-source, insights-agnostic self-driving product loop: ingest signals from the observability and support tools a team already uses, triage them into evidence-backed work orders, and turn the actionable ones into reviewed, test-passing pull requests — using the customer's own coding agent and compute. Self-host the full loop free, or use the hosted version.

## Problem Statement

Every vendor shipping an "AI fixes your bugs" loop couples it to their own data platform: PostHog self-driving requires your context in PostHog, Sentry Seer only fixes Sentry errors, Datadog Bits stays inside Datadog. Teams with existing tool sprawl (Sentry + PostHog + Zendesk + Linear is the norm) must either migrate their observability stack to get autonomous maintenance, or go without. Meanwhile the actual coding engine has commoditized — headless Claude Code produces a small fix PR for $1–5 in tokens against a $15/PR market price.

The unowned layer is **normalization + trust**: a neutral service that maps any signal source into a common schema, decides what is genuinely worth fixing, assembles the evidence, and gates output on quality so engineers trust the inbox instead of tuning it out.

**Cost of not solving it:** small teams keep paying maintenance in human hours or lock themselves into a single vendor's warehouse to escape it.

## Goals

1. **Parity of outcome with PostHog self-driving** for a team that does *not* centralize on PostHog: signals → reports → PRs → human merge, with equivalent evidence quality.
2. **≥60% PR merge rate** sustained over 30 days on Chalk (Phase 0 gate — this is the load-bearing validation).
3. **Source-agnostic by construction:** triage core never sees a vendor payload, only normalized Signals; adding an adapter requires zero changes to triage or runner.
4. **BYO agent + BYO compute:** customer code never transits Merge0 servers; the runner executes in the customer's CI/infra with their keys.
5. **Compounding trust telemetry:** merge/close/revert outcome of every PR captured from day one; acceptance rate is both the product metric and the sales asset.
6. **Open source as the growth engine:** the complete single-tenant loop is MIT and genuinely usable self-hosted; distribution comes from OSS adoption and community adapters, with hosted conversion as the revenue motion.

## Non-Goals

- **Hosting customer code or agents.** We orchestrate; we never clone customer repos onto our infrastructure. (Kills the enterprise objection; also keeps us out of the sandbox-fleet business PostHog had to build.)
- **Building capture/observability primitives.** No event SDK, no error grouping, no session replay. PostHog/Sentry do this; we consume their APIs.
- **Review-signal mining (LoopForge scope).** LoopForge remains a separate venture. A future LoopForge adapter is P2 architectural insurance only.
- **Desktop app or standalone CLI surfaces.** Web inbox + Slack only for v1. Parity of outcome, not surface area.
- **Feature development PRs.** v1 scope is maintenance: bug fixes, error handling, small chores. Net-new features are out of scope until merge-rate trust is established on fixes.
- **Auto-merge.** Nothing ships without a human clicking merge. Ever. This is a product principle, not a v1 limitation.
- **Open skill/MCP marketplace.** Third-party skill submissions are a supply-chain and prompt-injection surface we will not own before merge-rate trust is proven. v1 skills are git-native in the customer's repo; a *curated, signed* registry is P2. See Agent Extensibility.

## Target Users & Personas

| Persona | Description | What they get |
|---|---|---|
| **Platform/DevEx lead** (primary buyer) | 20–200 person company, Sentry + PostHog/Datadog + support tool sprawl, no appetite to migrate stacks | Overnight maintenance PRs without vendor lock-in or code leaving their infra |
| **Solo founder / tiny team** (design partner profile) | 1–3 people running a SaaS (e.g., Chalk) | Support N customers without hiring; wake up to reviewable diffs |
| **Reviewing engineer** (daily user) | Whoever owns the inbox | High-precision reports with evidence links; PRs that pass CI before they see them |

## User Stories

Ordered by priority.

1. As a **platform lead**, I want to connect our existing Sentry and PostHog projects in under 15 minutes so that we get value without migrating any data.
2. As a **reviewing engineer**, I want each report to bundle its evidence (stack trace, replay link, affected-account count, first-seen release) so that I can approve or dismiss it in under two minutes.
3. As a **reviewing engineer**, I want every generated PR to have passed the repo's test suite in CI before it reaches me so that reviewing it costs less than writing the fix myself.
4. As a **platform lead**, I want the runner to execute in our own GitHub Actions with our own Anthropic key so that source code never touches Merge0's servers.
5. As a **reviewing engineer**, I want to dismiss a report with a reason ("intended behavior", "won't fix", "duplicate") so that triage stops resurfacing it and learns from the verdict.
6. As a **platform lead**, I want a weekly Slack digest of reports awaiting review and PRs merged so that the loop stays visible without another dashboard to check.
7. As a **founder**, I want the system to notice when one of its own merged PRs is reverted so that it logs a hard negative and doesn't repeat the pattern.
8. As a **platform lead**, I want per-repo intent docs (invariants, "this is a feature not a bug" notes) injected into triage so that false-positive work orders stay rare.

## Architecture

Five components. The Signal schema is the contract between them and the real IP.

```
┌───────────┐   ┌───────────────┐   ┌─────────┐   ┌──────────┐   ┌────────┐
│ Adapters  │──▶│ Context Store │──▶│ Triage  │──▶│  Runner  │──▶│ Inbox  │
│ (ingest)  │   │ (assembly)    │   │ (scouts │   │ (BYO     │   │ (+Slack│
│           │   │               │   │ + gate) │   │  agent)  │   │ digest)│
└───────────┘   └───────────────┘   └─────────┘   └──────────┘   └────────┘
                        ▲                                            │
                        └────────────── outcome memory ◀─────────────┘
```

### 1. Ingestion Adapters

Each adapter polls or receives webhooks from a source and emits normalized **Signals**. Adapters are isolated crates/modules; the core never imports vendor types.

- **Phase 0 adapters:** PostHog (error tracking issues, dead/rage-click sessions, event funnels via HogQL/API) and Sentry (issues, releases, event payloads).
- **P1 adapters:** Zendesk/Intercom (support tickets), GitHub Issues, generic webhook + OTel endpoint.
- Adapter conformance is testable: golden vendor payloads in, expected Signals out.

### 2. Signal Schema (load-bearing spec)

```
Signal {
  id:            ULID
  source:        enum { posthog, sentry, zendesk, github, webhook, ... }
  source_ref:    string        // vendor-native ID + deep link
  kind:          enum { exception, ux_friction, ticket, regression, custom }
  severity:      enum { low, medium, high, critical }
  title:         string
  body:          string        // normalized description
  evidence:      [EvidenceLink]  // replay URL, stack trace, ticket thread
  fingerprint:   string        // stable hash for dedupe within a source
  join_keys: {                 // correlation context — REQUIRED where derivable
    release:     string?       // semver or SHA
    stack_hash:  string?
    account_id:  string?
    url_path:    string?
  }
  affected_count: int?         // users/accounts impacted
  first_seen:    timestamp
  last_seen:     timestamp
  raw:           jsonb         // original payload, for audit only
}
```

Design rules:

- `join_keys` are what let triage correlate a Sentry exception with a PostHog rage-click and a support ticket describing the same bug. They are specified here, not bolted on later.
- Adapters may not invent fields; extensions go through schema versioning.
- The schema is published/documented — it is also the integration surface for the generic webhook adapter.

### 3. Context Store

A thin assembly layer, **not** a warehouse. Four context types:

| Context | Source | Storage |
|---|---|---|
| **Intent** — what the product is supposed to do | Per-repo docs pack: CLAUDE.md, invariants file, feature notes (customer-authored, onboarding step) | Git (customer repo path, fetched at run time) |
| **Correlation** — cross-source joins | `join_keys` on Signals | Postgres (Signal table + join queries) |
| **Release** — deploy timeline, changelog | GitHub Releases API + deploy webhook | Postgres (`releases` table) |
| **Outcome memory** — verdicts and PR fates | Inbox verdicts; GitHub PR webhooks (merged / closed / reverted) | Postgres (`outcomes` table) |

Outcome memory feeds back into triage scoring ("we tried this in March and it was reverted") and is the compounding moat: it only accumulates from running the loop.

### 4. Triage: Scouts + Gate

**Scouts** — scheduled agents, each a standing question defined as config (prompt + source query template), not code:

- New exception clusters this period?
- Any release with anomalous error-rate delta? (release context join)
- Rising drop-off on a tracked funnel?
- Cross-source correlation: same fingerprint/join_keys appearing in ≥2 sources?

Scout output: candidate findings.

**Clustering/dedupe pass** — cheap model (Haiku-class): merge findings across scouts and sources into a **Report** with assembled evidence.

**Gate** — the quality bar. Per Report, a gate prompt evaluates against intent context + outcome memory:

> Is this a low-risk, well-specified maintenance fix with a clear repro and clear success criterion? Output a **Work Order** or **SKIP** with reason.

Two properties of that context are load-bearing, and both are enforced in code rather than left to budget arithmetic:

- **Intent is retrieved, not truncated.** The gate's character budget selects *whole* MERGE0.md sections by relevance to the Report — machine-managed amendments first (§5c mechanism 3 writes there, so a constraint earned from a real incident always reaches the decision), then sections sharing vocabulary with the Report, with a boost for rule-shaped headings. Whatever does not fit is **named** in the prompt, and the gate is instructed to SKIP rather than guess when an unseen section plausibly governs the Report. Deciding on partial intent is acceptable; not knowing that intent is partial is not.
- **Outcome memory decays.** Prior attempts are rendered with their age, and those older than `stale_prior_days` are marked STALE: they inform the decision but do not veto it. Memory without recency permanently forecloses retrying anything that once failed against a codebase that no longer exists.

```
WorkOrder {
  report_id:      ULID
  repo:           string
  summary:        string
  evidence:       [EvidenceLink]
  repro:          string
  suspect_change: string?      // "regressed in v2.3, likely PR #412" from release context
  success_criteria: string     // testable
  constraints:    string       // from intent docs
  prior_attempts: [OutcomeRef] // from outcome memory
}
```

Gate precision is tunable and must start conservative: a quiet inbox that's always right beats a busy inbox that's sometimes right.

**Evidence budgets:** Work Order assembly enforces explicit caps — max evidence items, max character budget per section, and assembly timeout — so a noisy incident can never produce a Work Order that drowns the agent's context. Over-budget evidence is truncated with deep links back to the source, never silently dropped.

### 5. Runner (BYO agent, BYO compute)

- **v1 agent:** Claude Code headless (`claude -p` / Agent SDK). The runner interface is agent-agnostic (`WorkOrder in → PRResult out`) so alternatives are a P2 config change, not a rewrite.
- **Execution model:** a GitHub Actions workflow (or equivalent) installed in the *customer's* repo/org. Merge0 dispatches a Work Order via repository_dispatch; the workflow clones, runs the agent with the customer's Anthropic key, runs the test suite, and opens the PR via `gh`.
- **Diff budget:** Work Orders carry a maximum change footprint (default: single concern, small file count, small line delta — exact defaults tuned in Phase 0). Empirical studies of agent-authored PRs show small, narrowly scoped changes merge at dramatically higher rates while large multi-file changes die in review; a run whose fix exceeds the budget discards itself with a "fix larger than expected" outcome, which is itself triage signal (the report was under-specified or the defect is architectural).
- **Failed-run salvage:** when a run self-discards (test failures exhausted the repair budget, or diff budget exceeded), the agent's root-cause investigation attaches to the originating Report before teardown. A discarded run still saves the human who picks it up the diagnosis time — failure produces evidence, not nothing.
- **Self-repair budget, then self-discard:** a failing test run is a repair signal before it is a discard trigger. The runner grants the agent a bounded number of repair iterations (default 3) — read the precise failure, fix, re-run — before the run discards itself and reports failure to outcome memory. No red PRs reach the inbox either way; the budget exists to raise runner yield, not to lower the bar. ("You only pay for real work" as an enforced invariant.)
- **Safety guarantees** (all customer-side settings, verified at onboarding): branch protection on default branch, PRs from a dedicated bot branch namespace, CI required, no auto-merge, agent tool allowlist scoped to edit/test/git.
- **Secrets:** runner never receives customer secrets beyond what the workflow's own environment provides; Work Orders are sanitized before dispatch (no raw payloads).

### 5a. Git Auth Model

Two credential paths, deliberately separate:

**Server-side (Merge0 → GitHub):** Merge0 is a **GitHub App**. All API access (webhooks, `repository_dispatch`, PR/release reads, safety verification) uses short-lived **installation access tokens** scoped to the installed repos. No PATs, no OAuth user tokens, no long-lived credentials at rest. Token minting is per-request; nothing cacheable beyond GitHub's ~1h token lifetime.

**Runner-side (agent → git):**

- **v1 (BYO Actions):** no proxy needed. The workflow's ephemeral `GITHUB_TOKEN` (permissions declared in the workflow file: `contents: write`, `pull-requests: write`, nothing else) handles clone, branch push, and `gh pr create`, and dies with the job. Merge0 never touches it.
- **P2 (non-Actions runners — customer VMs, other CI):** Merge0 adds a **credential broker**. Flow: runner authenticates to the broker with a per-tenant runner key → requests credentials for a specific Work Order → broker mints a GitHub App installation token scoped to that single repo with ~10-minute TTL → token is delivered to the agent via a **git credential helper**, so the raw token never enters the agent transcript, prompt context, or logs (same opaque-handle principle as the secret-vault pattern). Broker denies requests for repos not referenced by an approved Work Order. Interface is designed now; broker is built in P2.

Invariant across both paths: **the agent never sees a credential that outlives its run or exceeds its Work Order's repo scope.**

### 5b. Agent Extensibility: MCPs & Skills

Customers will want the agent to carry extra capability (an internal-API MCP, a database-schema MCP, house-style skills). Governing rule: **configuration transits Merge0; credentials never do.**

**Manifest (`.merge0/agent.toml`, in the customer repo):**

```toml
[[mcp]]
name = "internal-api"
command = "npx our-api-mcp"
auth_env = "INTERNAL_API_KEY"   # name only — resolved from the
                                 # customer's own CI secrets at runtime

[skills]
path = ".merge0/skills/"         # git-native skill directory

[network]
egress_allow = ["api.internal.example.com"]
```

- **Secret handling:** `auth_env` entries are *references by name*. The customer's Actions secrets (or VM env in P2) resolve them inside the job. Merge0 validates manifest schema and stores the manifest, but a credential value never transits or rests on Merge0 infrastructure — resolution is entirely customer-side, which gives us the wizard-style opaque-ref guarantee for free.
- **Sandboxing:** MCP servers run as subprocesses inside the same throwaway runner container, inherit its lifecycle, and are constrained by the manifest's egress allowlist (enforced in the runner harness). No manifest change takes effect except through the customer's own PR review — the trust model for agent config is identical to the trust model for their code.
- **Attribution:** the runner records which MCPs/skills were active per Work Order into outcome memory. This makes extension quality *measurable* ("PRs run with the DB MCP merge at 80%; without, 55%") and is the prerequisite for any future registry curation.

**Skills — v1 vs. later:**

- **v1 (git-native):** skills live in `.merge0/skills/` in the customer repo. Authored or copied in by the customer, reviewed via their normal PR process. No Merge0 UI beyond displaying which skills a Work Order ran with.
- **P2 (curated registry):** a Merge0-reviewed, signed skill registry, opt-in per repo from the inbox UI — install writes a manifest-change PR to the customer's repo (never a silent server-side toggle). Registry listings carry per-skill acceptance-rate telemetry.
- **Non-goal (restated):** an open marketplace with third-party submissions. Unvetted skills are prompt-injection vectors into an agent that writes code; we do not take on that vetting burden before the core loop has earned trust, and any eventual marketplace must be gated on per-skill outcome telemetry.

### 5c. Hardening Pass (prevention layer) — P1

Fixing a bug once is table stakes; retiring its defect class is the retention feature. When a Work Order's PR merges, an asynchronous **hardening pass** evaluates whether the fixed defect class is mechanically preventable and, if so, emits a *separate* follow-up PR — same gate, same inbox, same human-merge rule, categorized `hardening`.

**Enforcement hierarchy** — always prefer the most deterministic mechanism the defect class supports:

1. **Lint/AST rule** (eslint custom rule, ast-grep pattern, clippy lint) — CI-enforced, catches human and agent contributors alike, zero runtime cost.
2. **Regression test** — when the pattern is behavioral rather than syntactic.
3. **Intent-doc amendment** (MERGE0.md/CLAUDE.md) — fallback only, for constraints expressible solely as guidance. Amendments are scoped and periodically pruned; an ever-growing "don't do X" list is context rot, not prevention.

**Targeting:** outcome memory drives the queue. Recurring Signal fingerprints (same defect class fixed more than once) are top priority; single-occurrence fixes get a hardening PR only when the rule is trivially derivable from the diff.

**Design rules:**

- A hardening PR never piggybacks on the fix PR — reviewability and revert-independence require separation.
- **Fence rule for intent-doc amendments:** when the hardening pass writes to MERGE0.md/CLAUDE.md, it may only write inside an explicitly marked machine-managed section (fenced block); customer-authored prose is never rewritten by any Merge0 process. This is enforced in the runner harness, not left to prompt discipline.
- Every emitted rule embeds a reference to the originating Report/fingerprint, so a future rule-removal PR can be traced to what it re-exposes.
- Effectiveness is measured, not assumed: post-merge, the originating fingerprint's recurrence rate is tracked; a merged hardening PR whose fingerprint recurs is a hard negative in outcome memory.

**LoopForge boundary (explicit):** LoopForge mines *human review comments* into deterministic rules; Merge0's hardening pass mines *Merge0's own fixed defects* into rules. Different signal source, same artifact type. The ventures remain separate; the rule-synthesis step (defect/comment → lint rule) is a candidate shared library if both mature, and any convergence beyond that is a deliberate future decision, not drift.

### 5d. Self-Improvement Discipline

One invariant governs every learning mechanism in Merge0 (outcome memory, hardening, gate tuning, and any future meta-loop): **the system improves its artifacts, never itself.** No automated process modifies its own executor — the runner harness and base prompts are immutable within a release and change only through Merge0's own versioned releases. All learned improvement lands exclusively in supplemental, git-visible, human-merged artifacts: lint rules, tests, fenced intent-doc sections, skills, and scout/gate configuration. Every such change is evidence-linked (which outcomes motivated it) and rollbackable through ordinary git history. In-session self-modification is rejected by design: runs must be reproducible and auditable, which is incompatible with an agent that rewrites its own operating state mid-flight.

**Meta-loop (P2 — Merge0 on Merge0):** scout and gate prompts live as config files in a repo from day one. Merge0's own operational telemetry (gate precision, dismissal-reason distribution, runner yield, hardening effectiveness) is ingested as just another Signal source, and a **meta-scout** proposes evidence-linked config-change PRs to that repo — e.g., "dismissals for 'intended behavior' rising three weeks running on sync-related reports; proposed gate amendment attached." Same gate, same inbox, same human merge, full rollback. Self-tuning with zero new trust machinery, because the improvement loop is the product loop.

### 6. Inbox + Slack

- **Web inbox (v1):** single table view — Report status, severity, evidence links, gate reasoning, "Approve → dispatch Work Order" and "Dismiss (reason)" actions. Reports and resulting PRs linked bidirectionally.
- **Slack (v1):** notification on new Report and PR-ready events with approve/dismiss actions; weekly digest.
- Dismissal reasons are structured (intended behavior / won't fix / duplicate / bad evidence) and write to outcome memory.

### 6a. UX Requirements

Usability here is workflow trust, not visual polish, so requirements are stated as testable constraints rather than designs. Surface priority is ordered by actual reviewer time spent — and the highest-traffic interface is not the web app:

1. **The PR description (primary surface).** A standardized template: one-line summary, evidence links (replay, stack trace, originating Report), gate reasoning ("why this was judged safe to attempt"), what changed, what the tests verify, diff footprint vs. budget. Constraint: a reviewer must reach an approve/reject decision from the PR page alone, without opening the inbox. The template is versioned config (meta-loop-eligible).
2. **Slack messages.** One report or PR per message; decision-ready (severity, affected count, one-line evidence summary, approve/dismiss actions inline); deep link as fallback, never as requirement.
3. **Onboarding (MERGE0.md + workflow install).** Constraint from user story 1: first connected repo to first Signal ingested in under 15 minutes; MERGE0.md starts from a shipped template with fenced machine sections pre-marked.
4. **Web inbox.** A review queue, not a dashboard: newest-first, one-screen decision per report (evidence above the fold), keyboard-actionable approve/dismiss, no configuration surfaces mixed into the review path. Anything that grows the time-to-review median is a regression regardless of how useful it looks.

**Research method by phase:** Phase 0–1 is instrumented dogfooding — median time-to-review (<10 min target) and per-report triage time (<2 min target) are the usability metrics, reviewed weekly alongside gate precision. Formal moderated sessions begin at Phase 2 with the second design partner; no journey maps or wireframe deliverables before then. Launch-time visual polish for the OSS release is a Phase 1 line item (the inbox is part of the credibility pitch), scoped to the frontend-design pass, not a design system.

## Stack & Deployment

- **Service:** Rust / Axum, Postgres (schema-per-tenant, consistent with existing AdminRemix patterns), deployed via Coolify on DigitalOcean for Phase 0–1.
- **Scheduling:** cron-driven scout runs (nightly default, per-tenant configurable).
- **Customer-side:** one GitHub App install (webhooks: PRs, releases) + one Actions workflow file + adapter API keys.
- **Tenancy for Phase 0:** single tenant (Chalk). Multi-tenant hardening is Phase 2.

## Distribution & Open Source Strategy

**Model:** open-core, PostHog-style. One public repo: MIT everywhere except an `/ee` directory under a commercial license. The hosted version is the `/ee` build operated by AdminRemix.

### The split

Governing rule for what goes where: **the MIT core must be a complete, honest single-tenant product.** A crippled self-host offering forfeits the trust that is the entire point of open-sourcing this — the DevEx buyer adopts because they can read the runner harness and verify their code never leaves their infra.

| MIT core (self-hostable, complete) | `/ee` + hosted |
|---|---|
| All adapters + Signal schema + golden-payload test framework | Multi-tenant org management, SSO/SAML, RBAC, audit log |
| Context store (all four context types, incl. outcome memory) | **Cross-tenant outcome priors** — anonymized, aggregated "what kinds of fixes merge" intelligence that improves gate precision (hosted-only *data service*, not just gated code) |
| Scouts, clustering, gate | Curated signed skill registry (P2) |
| Runner harness, manifest support, hardening pass | Credential broker as a managed service (self-hosters can run the OSS broker themselves in P2) |
| Inbox + Slack surface (single-org) | Billing, usage metering, hosted convenience (managed upgrades, backups) |

The moat analysis already in this document survives open-sourcing untouched: outcome memory only accumulates from *operating* the loop, cross-tenant priors only exist on the hosted side, and the acceptance-rate track record is earned, not cloned. The code was never the moat; publishing it is how we get distribution the adapters need.

### Growth motion

1. **Launch with proof, not promise:** the repo goes public *after* the Phase 0 gate is met, so the README leads with "ran against our own production SaaS for N days, X% of PRs merged" and links the actual merged PRs — the same credibility device as PostHog's merged-PR wall.
2. **Adapters are the contribution surface:** the Signal schema + golden-payload conformance tests make a community adapter a well-defined, testable PR. Each merged adapter is also a GTM page ("Datadog → PRs"), reusing the Chalk competitor-page playbook.
3. **Content flywheel:** building in public — Chalk as the living case study ("how two people support N school districts"), gate-precision write-ups, the hardening/extinction metric.
4. **Conversion path:** self-hosters convert on multi-tenant/SSO needs, cross-tenant gate intelligence, and not wanting to operate another service — never on artificial core limitations.

### Mechanics

- **CLA required** from external contributors (dual-licensing `/ee` compatibility depends on it). Lightweight CLA-assistant bot, not paperwork.
- **Trademark** on the final name held by AdminRemix; the brand, not the license, is what prevents confusing clones.
- **Self-host telemetry:** off by default with documented opt-in — same posture as self-hosted Chalk, and it must be, since the audience overlaps and inconsistency would be noticed. Hosted telemetry is on as part of the service.
- **Versioned public docs for the Signal schema** — it doubles as the webhook-adapter integration contract and the standard we want to own.
- **Support boundary stated in the README from day one:** GitHub issues/discussions for OSS, SLAs are hosted-only. Community support burden is a real cost; scope it before it scopes you.

## Requirements

### Must-Have (P0)

| # | Requirement | Acceptance criteria |
|---|---|---|
| P0-1 | PostHog adapter | Given a PostHog project with error tracking enabled, when the nightly sync runs, then new issues and flagged sessions appear as Signals with populated `join_keys` where derivable; golden-payload tests pass |
| P0-2 | Sentry adapter | Same as P0-1 for Sentry issues; `release` join_key populated from Sentry release data |
| P0-3 | Signal store + dedupe | Given the same underlying defect reported by both sources, when triage runs, then exactly one Report exists referencing both Signals |
| P0-4 | Release context | Given GitHub Releases exist, when a Report is generated for a regression, then it includes first-bad-release attribution where the data supports it |
| P0-5 | Gate → Work Order | Given a Report, when the gate runs, then output is a schema-valid Work Order or a SKIP with a stated reason; no Work Order is emitted without testable success criteria |
| P0-6 | Runner dispatch + self-discard | Given an approved Work Order, when dispatched, then a PR is opened only if the full test suite passes in the customer workflow; failed runs write a failure outcome and open nothing |
| P0-7 | Inbox | Given pending Reports, when the reviewer opens the inbox, then they can approve or dismiss with a structured reason; verdicts persist to outcome memory |
| P0-8 | Outcome capture | Given a Merge0 PR, when it is merged, closed, or its commits are reverted within 14 days, then the outcome (including revert-as-hard-negative) is recorded and queryable |
| P0-9 | Safety verification | Given onboarding, when a repo is connected, then Merge0 verifies branch protection + required CI and refuses to dispatch until both are confirmed |
| P0-10 | Acceptance-rate telemetry | Merge rate, dismissal-reason distribution, and time-to-review are computed continuously and visible on an internal dashboard from the first PR |
| P0-11 | GitHub App auth invariants | Given any Merge0 server-side GitHub API call, when audited, then it used a short-lived installation token scoped to installed repos — no PATs or user OAuth tokens exist in the system; given a runner job, when its logs and agent transcript are inspected, then no git credential appears in either |

### Nice-to-Have (P1)

- Slack approve/dismiss actions and weekly digest (inbox-only is acceptable for the first two weeks of Phase 0).
- Zendesk/Intercom and GitHub Issues adapters.
- Generic webhook + OTel adapter with published Signal schema docs.
- Intent-doc onboarding wizard (v0 is "commit a MERGE0.md; here's the template").
- Per-scout precision tuning UI (v0 is config file).
- `.merge0/agent.toml` manifest support in the runner: custom MCP declarations with `auth_env` name-only refs, git-native skills directory, egress allowlist enforcement, per-Work-Order MCP/skill attribution to outcome memory. (P0 runs with a fixed default toolset; Chalk itself is the first manifest consumer.)
- **Hardening pass (§5c):** post-merge prevention PRs following the lint > test > intent-doc hierarchy, targeted by fingerprint recurrence, effectiveness tracked in outcome memory. Ships immediately after the Phase 0 gate is met — the merge-rate validation must measure the fix loop alone.

### Future Considerations (P2)

- Agent-agnostic runner configs (Codex CLI, others) — interface designed for this now, alternatives not built.
- **Credential broker** for non-Actions runners: per-Work-Order, single-repo, ~10-min-TTL GitHub App tokens delivered via git credential helper (see §5a; interface designed now, built P2).
- **Curated skill registry:** Merge0-reviewed and signed, opt-in from the inbox UI, installation delivered as a manifest-change PR to the customer repo, listings gated on per-skill acceptance telemetry. Open third-party marketplace remains a non-goal.
- **Meta-loop (§5d):** Merge0's own telemetry as a Signal source; meta-scout proposes evidence-linked scout/gate config changes as PRs through the standard gate and inbox. Prerequisite (P0-cheap): scout/gate prompts are config files in a repo from day one, so the meta-loop later requires no re-architecture.
- **Opportunity Reports (divergent signal, convergent boundary):** a report category for clustered *demand* evidence — feature-request tickets, funnel drop-offs at missing affordances, dead/rage clicks on non-interactive elements, and recurring "intended behavior" dismissals (users repeatedly colliding with the design). Opportunity Reports carry evidence and affected-count like any report but produce **no Work Order and no PR**; their only terminal action is human handoff (evidence brief → spec process or interactive agent session). The system detects demand autonomously; it never initiates feature creation — that boundary is a product principle, consistent with the feature-development non-goal.
- LoopForge adapter (review-signal mining as a Signal source) — architectural insurance only; ventures remain separate.
- Multi-tenant self-serve onboarding + billing (per-merged-PR pricing below $15, or flat monthly with PR pool — final model is an open question).
- Datadog adapter.
- Report-level cost accounting (tokens per merged PR) surfaced to customers.

## Success Metrics

**Phase 0 gate (blocking):** ≥60% of dispatched PRs merged over a rolling 30-day window on Chalk, with ≥10 PRs in the window. If unmet after 60 days of tuning, stop and reassess before any multi-tenant work.

**External calibration for the 60% number:** Sentry's Autofix — running broadly on incoming issues — reports a 41→46% merge rate; Linear's internal loop lands correct one-shot fixes ~33% of the time on messy, varied failures, and improved by adding prove-you-understand gates. 60% is above both, and is achievable only because Merge0 gates *before* dispatching: the gate's job is to decline the work those systems attempt and miss. If Phase 0 lands in the 45–60% band, the correct response is tightening gate precision (fewer, better Work Orders), not relaxing the target — the empirical two-regime finding (small scoped PRs merge near-instantly; sprawling ones die in review) says selectivity, diff budgets, and evidence quality are the levers.

Leading indicators (evaluate weekly during Phase 0):

- Gate precision: ≥70% of Work Orders approved by reviewer (dismissals below 30%).
- Runner yield: ≥50% of dispatched Work Orders produce a test-passing PR.
- Time-to-review: median under 10 minutes per PR (proxy for evidence quality).
- False-positive Reports (dismissed as "intended behavior"): trending down week over week.

Lagging indicators (evaluate at 90 days):

- Chalk support burden: measurable reduction in founder hours on maintenance (self-reported log).
- Revert rate on Merge0 PRs: <5%.
- Fingerprint recurrence after a merged hardening PR: approaching zero (Phase 1 onward — the "defect classes go extinct" claim, measured).
- Acceptance-rate telemetry robust enough to appear in sales material ("X% of our PRs get merged").

Post-launch OSS indicators (Phase 1 launch + 90 days):

- Self-host installs reaching first-PR-merged (opt-in telemetry or docs-based proxy): target 25.
- Community adapter PRs passing conformance: ≥2.
- Hosted signups from OSS funnel: track from day one; conversion targets set once baseline exists (no invented number).

## Phasing

**Phase 0 — Chalk end-to-end (validation gate).**
PostHog + Sentry adapters, Signal store, release context, scouts (3–4), gate, Claude Code runner in Chalk's Actions, minimal inbox, outcome capture. Exit: the 60% / 30-day / ≥10-PR gate above.

**Phase 1 — Trust + breadth on one tenant, then public launch.**
Slack surface, support-ticket adapter, hardening pass (§5c), intent-doc onboarding polish, revert detection hardening, gate tuning against accumulated outcome memory. Repo restructure for open-core (`/ee` boundary, CLA, schema docs) and **public launch at end of phase**, README leading with the Phase 0 merge-rate proof and Chalk case study.

**Phase 2 — Hosted version + community.**
Multi-tenant hardening and org management in `/ee`, hosted onboarding flow, pricing experiment, generic webhook adapter + published Signal schema docs, community adapter pipeline (conformance tests + GTM page per adapter: "Sentry → PRs", "Datadog → PRs"), cross-tenant outcome priors once ≥2 tenants exist.

No hard external deadlines. Sequencing dependency: scouts before breadth (scouts are only useful once the downstream pipeline consumes them — already satisfied by Phase 0 ordering).

## Risks

| Risk | Mitigation |
|---|---|
| Loop commoditizes within 12–18 months | Moat = outcome memory + acceptance-rate record + adapter network, all of which only accumulate from operation; instrumented from day one (P0-10) |
| Inbox trust collapse (noisy reports) | Gate starts conservative; precision is the tracked leading metric; structured dismissals feed back into scoring |
| Vendor API changes (PostHog/Sentry) | Adapter isolation + golden-payload conformance tests; core untouched by vendor churn |
| Runner damage to customer repos | BYO-compute + branch protection verification (P0-9) + self-discard + no auto-merge principle |
| Skill/MCP supply chain & prompt injection | Git-native config only in v1 (all changes pass customer PR review); credentials never transit Merge0 (name-refs resolved customer-side); egress allowlist per manifest; curated-and-signed registry before any UI-driven install; open marketplace is a standing non-goal |
| Cloud clone of the MIT core | Accepted cost of the model. Moat is operational (outcome memory, cross-tenant priors, acceptance track record, brand/trademark) — none of it clonable from the repo; `/ee` holds the multi-tenant machinery a hosting competitor needs most |
| Community support burden drowns a two-person company | Support boundary in README from day one (issues/discussions only for OSS); conformance tests make adapter PRs largely self-reviewing; SLAs are hosted-only |
| Reviewer abandonment (the empirically #1 killer of agent PRs) | Only test-passing, diff-budgeted, single-concern PRs reach the inbox; Slack nudges + weekly digest keep pending PRs visible; time-to-review is a tracked leading metric; stale-PR aging surfaces in the digest so nothing rots silently |
| Chalk telemetry vs. K-12 privacy posture | Chalk-side concern but adjacent: hosted Chalk telemetry always-on and DPA-disclosed; self-hosted opt-in; replay masking on any student-PII surface (tracked in Chalk's own docs, referenced here for the design-partner integration) |

## Roadmap (post-Phase-0 candidates, design-sketched)

Two market asks are acknowledged and deliberately deferred; both are
architecturally large enough to be their own projects, so they are named
here with a design direction rather than half-built.

### GitLab support (forge abstraction)

Today the forge surface is `merge0-github`: App-token auth, dispatch,
branch-protection safety checks, PR lifecycle webhooks, release timeline.
The design direction is a `Forge` trait extracted from the current
`GitHubApi` (dispatch, default_branch, branch_protection,
create_branch_with_files, create_pull_request/MR, get_file_content,
list_releases, webhook verification) with `GithubForge` as the first impl
and `GitlabForge` as the second (Merge Requests, pipeline status, project
access tokens or CI job tokens for the runner path, `X-Gitlab-Token`
webhook verification). The runner's `repository_dispatch` becomes a
forge-specific job trigger (GitLab: pipeline trigger tokens). Everything
above `merge0-github` — triage, store, server, UI — already speaks in
forge-neutral types (`RepoRef`, `PrInfo`, Work Orders), so the trait
extraction is the bulk of the work, not a rewrite.

### SSO / SAML (ee)

The MIT core stays single-token by design (one operator, one bearer). The
hosted control plane (`ee/merge0-hosted`) is where identity lands:
OIDC-first (SAML via bridge), login exchanging the IdP assertion for a
short-lived session bound to a tenant + RBAC role (the existing
`ee/merge0-ee/src/rbac.rs` matrix), group-claim → role mapping, and the
audit log gaining actor identity from the session rather than the
`x-merge0-actor` header. No identity tables in the MIT store; enterprise
identity is an `/ee` concern end to end.

## Open Questions

| Question | Owner | Blocking? |
|---|---|---|
| Product name ("Merge0" is a placeholder) | Founder | **Blocking for public repo/launch** (Phase 1 end); trademark search before committing |
| CLA tooling and terms (individual + corporate) | Founder + counsel | Blocking for accepting external PRs (Phase 1 end) |
| Pricing model for hosted (per-merged-PR vs. flat + pool) | Founder | Non-blocking (Phase 2) |
| Sentry cloud vs. self-hosted GlitchTip for Chalk's own instance | Founder + eng | Non-blocking (adapter targets Sentry API either way; confirm GlitchTip API parity before committing) |
| PostHog Cloud vs. self-hosted for Chalk hosted tier (data-residency story vs. ops burden) | Founder | Non-blocking for Merge0; decide during Chalk instrumentation |
| Minimum intent-doc requirement — is MERGE0.md mandatory at onboarding or optional with degraded gate precision? | Eng | Resolve during Phase 0 |

---

*Related but separate documents: Chalk PostHog instrumentation taxonomy (districts/schools/roles/sync-jobs as groups) — to be authored before Phase 0 adapter work lands, since join_key quality depends on it.*
