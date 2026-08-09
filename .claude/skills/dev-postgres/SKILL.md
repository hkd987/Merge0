---
name: dev-postgres
description: Start/stop the throwaway local Postgres that the workspace tests and manual e2e expect on port 55432. Use whenever tests fail with a connection error.
---

# Dev Postgres

The workspace tests, `scripts/e2e-manual.sh`, and `scripts/verify.sh` all
expect `postgres://merge0@localhost:55432/merge0`. `scripts/dev-pg.sh`
owns a throwaway cluster on that port so nothing touches a system
Postgres:

```sh
scripts/dev-pg.sh start    # init (first time) + start + create db; prints PG_OK <url>
scripts/dev-pg.sh status   # PG_OK <url>, or non-zero when down
scripts/dev-pg.sh stop     # fast shutdown
scripts/dev-pg.sh destroy  # stop + delete the data dir
```

Notes:

- Works as a normal user (data in `~/.merge0/pg`) or as root in a
  container/CI sandbox (delegates to the `postgres` system user, data in
  `/var/lib/merge0-pg/data`). Override with `MERGE0_PG_DATA`,
  `MERGE0_PG_PORT`, `MERGE0_PG_BIN`.
- Auth is `trust`, loopback-only, holding invented fixture data — a dev
  convenience, never a production pattern.
- Tests provision throwaway schemas per run; if a crashed run leaves
  orphans behind, `scripts/dev-pg.sh destroy && scripts/dev-pg.sh start`
  is the clean reset.
- Stop it when you're done (standing cleanup rule): an idle daemon plus a
  13 GB `target/` has OOM-killed a test run here before.
