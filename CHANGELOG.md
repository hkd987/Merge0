# Changelog

Curated, user-facing changes per release. Full detail lives in the git
history and each release's generated notes.

## v0.1.0 — 2026-08-09 (first tagged release)

The complete single-tenant loop, measured and manually verified end to
end (81-check e2e; 30-scenario live gate corpus at 100% with zero canary
leaks; six seeded-bug agent fixtures at 5 fixes + 1 refusal).

### The loop
- 17 signal sources normalized into one versioned Signal schema (v0.7):
  errors/product analytics (PostHog, Sentry, Datadog, LoopForge, OTel,
  Mixpanel, OpenPanel), ticketing/planning (Zendesk, Intercom, GitHub
  Issues, Jira, Linear, Asana, Trello, Slack channels), social feedback
  (Reddit, X), plus a generic webhook envelope. Pollers with cursors and
  fail-loud misconfiguration; signature-verified native webhooks.
- Config-driven scouts → deterministic cross-source clustering → a
  model gate that reads the repo's own MERGE0.md intent doc and outcome
  memory, emitting evidence-backed Work Orders with testable success
  criteria and self-assessed confidence — or reasoned skips. Never
  silence.
- Keyboard-first inbox + report detail (confidence, dispatch audit
  trail, CODEOWNERS routing, fix efficacy), acceptance dashboard,
  one-page onboarding. Slack notifications and interactive approvals.
- BYO-agent runner for six harnesses (Claude Code, Codex CLI, Gemini
  CLI, Aider, OpenCode, Cursor CLI, or any custom command) on the
  customer's own CI with their own keys; diff budgets enforced
  server-side; repair-or-self-discard; no red PRs.
- Outcome memory closes the loop: merges/closes/reverts (webhook + a
  reconciliation sweep that repairs missed deliveries), fix-efficacy
  tracking, delivery as Jira stories for teams not ready for autonomous
  code.

### Trust posture
- A human merges every change; auto-dispatch exists but ships off.
- Confidence routing only ever downgrades (unconfident work → tracker
  story, never a gambled PR).
- Hard token budget with a rolling window; X reads spend-capped per
  round; gate failures isolate per report with an outage circuit.
- Secrets by env-var name only; work-order sanitization; exploit and
  credential canaries enforced by live evals.

### Operations
- Single binary + Postgres; Docker/compose; one-click Render, Railway,
  Fly configs; ee/ multi-tenant control plane (tenant lifecycle, RBAC,
  audit, metering, cross-tenant priors).
- `merge0 triage` CLI quickstart: one vendor export in, gate verdicts
  out — no server, no database, no GitHub App.
