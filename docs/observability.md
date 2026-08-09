# Observability

Merge0 exports its loop health on `GET /metrics` in Prometheus text
format. The endpoint sits on the protected router — the scraper must send
the same bearer token as the API.

The design bias: **a self-driving loop fails silent**. A wedged scheduler,
an expired vendor credential, or an exhausted token budget all look, from
the outside, like a quiet week — every counter simply stops moving. So the
pack's primary signals are liveness timestamps you alert on the *age* of,
not volume counters.

## Scrape configuration

```yaml
# prometheus.yml
scrape_configs:
  - job_name: merge0
    metrics_path: /metrics
    scheme: https
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/merge0-token   # MERGE0_API_TOKEN
    static_configs:
      - targets: ["merge0.internal:8080"]
rule_files:
  - alerts.yml   # ops/alerts.yml from this repo
```

Any 15–60s scrape interval is fine; the interesting series move at
triage-run cadence.

## The metrics

### Liveness (alert on these)

| Series | Meaning |
|---|---|
| `merge0_last_triage_run_timestamp_seconds` | Unix time the most recent triage run started. **Absent until the first run** — deliberately, so "never ran" cannot masquerade as 1970. Alert when `time() - value` exceeds ~2x your `MERGE0_TRIAGE_INTERVAL_SECS`. |
| `merge0_fetch_last_run_timestamp_seconds{source=}` | When each source's poller last completed a round (persisted, survives restarts). A source going stale means expired credentials or vendor API drift — its signals are silently missing from triage. |
| `merge0_fetch_failures_total{source=}` | Failed poll rounds per source since process start. In-memory by design: counter resets on restart are normal Prometheus semantics (`increase()` handles them). Zero-series are emitted for every enabled source so the first failure is a visible step, not a new series. |

### Spend

| Series | Meaning |
|---|---|
| `merge0_tokens_spent_24h` | Model tokens spent in the trailing 24h (gate + runner). |
| `merge0_token_budget_remaining` | Tokens left in the rolling daily budget. Only exported when `[budget] max_tokens_per_day > 0` in `config/gate.toml`. 0 = the gate is paused until the window rolls. |

### Loop outcomes (30-day window)

`merge0_work_orders_dispatched`, `merge0_prs_opened`, `merge0_prs_merged`,
`merge0_prs_closed`, `merge0_prs_reverted`, `merge0_runs_discarded`,
`merge0_merge_rate`, `merge0_runner_yield`, `merge0_gate_precision`,
`merge0_tokens_per_merged_pr`, `merge0_phase0_gate_met`,
`merge0_fixes_confirmed`, `merge0_fixes_recurred`,
`merge0_fix_efficacy_rate`, `merge0_auto_dispatched`, and live queue
depths in `merge0_reports{status=}`. Rate-style series are omitted until
their denominator exists (no fake zeros).

## Dashboard and alerts

- **`ops/grafana-dashboard.json`** — import via Grafana → Dashboards →
  Import; it prompts for your Prometheus datasource. Top row is the
  heartbeat (time since last triage run, merge rate, runner yield, fix
  efficacy, 24h spend); below are queue depths, PR outcomes, poller
  freshness, and poller failures.
- **`ops/alerts.yml`** — Prometheus alerting rules. Thresholds assume the
  default nightly triage interval; if you shorten
  `MERGE0_TRIAGE_INTERVAL_SECS`, tighten the two 26-hour staleness windows
  to roughly twice your interval. What pages vs warns:
  - `Merge0TriageLoopStalled` (page) — no triage run in 26h.
  - `Merge0TriageLoopNeverRan` (warn) — up but no run ever recorded.
  - `Merge0FetchSourceStale` / `Merge0FetchSourceFailing` (warn) — a
    source's signals are missing from triage.
  - `Merge0TokenBudgetExhausted` (warn) — gate paused on spend.
  - `Merge0FixesRecurring` (warn) — merged fixes are bouncing; review
    before approving more of the same class.

## What is deliberately not here

No tracing/OTLP exporter and no metrics client library — the server
hand-renders the exposition format because a handful of gauges does not
justify a dependency. If an install outgrows that (high-cardinality
per-tenant series on the hosted surface), that is an `ee/` concern.
