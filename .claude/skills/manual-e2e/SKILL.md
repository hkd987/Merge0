---
name: manual-e2e
description: Drive the real merge0-server binary over HTTP through the whole loop (ingest → triage → review → runner callbacks → telemetry, plus the hosted multi-tenant plane) with no external credentials. Use before shipping server/pipeline changes.
---

# Manual end-to-end run

`scripts/e2e-manual.sh` boots the real server binary with dev fakes
(fake model + fake GitHub — everything else is the shipped code path)
and drives ~70 checks over HTTP: auth sweeps, native vendor webhooks,
triage runs, approvals, runner callbacks, budgets, telemetry, the SPA,
and the hosted control plane with two isolated tenants.

```sh
scripts/dev-pg.sh start                      # Postgres on 55432
(cd ui && npm ci && npm run build)           # SPA checks need ui/dist embedded
cargo build -p merge0-server -p merge0-hosted
scripts/e2e-manual.sh                        # or: scripts/e2e-manual.sh <database_url>
```

Reading failures:

- **SPA checks 503** usually means `ui/dist` was empty when the server
  was built (rust-embed bakes it in at compile time) — rebuild the UI,
  then rebuild the server.
- **A signal silently missing from triage**: check its `last_seen`
  against the scout window — e2e payloads need *generated* now-relative
  timestamps, not values copied from fixtures.
- **Count assertions off after adding a source/scout**: expected — new
  sources change triage arithmetic. Update expectations by *content*
  (title match), never by index, and respect the gate's
  `max_work_orders_per_run` cap.

When extending the script: keep new checks substring-based via
`check <desc> <actual> <expected-substring>`, generate timestamps with
`date -u`, and never embed real vendor data or credentials.

Afterwards, standing cleanup: `scripts/dev-pg.sh stop`, `cargo clean` if
disk pressure, remove `ui/node_modules` if you installed it only for
this.
