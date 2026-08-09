#!/usr/bin/env bash
# Local dev Postgres for the Merge0 test suite and manual e2e.
#
# The workspace tests expect postgres://merge0@localhost:55432/merge0
# (override with MERGE0_TEST_DATABASE_URL). This script owns a throwaway
# cluster on that port so `cargo test --workspace` and
# scripts/e2e-manual.sh work without touching any system Postgres.
#
# Usage: scripts/dev-pg.sh init|start|stop|status|url|destroy
#
# Overrides (env):
#   MERGE0_PG_DATA  data directory  (default: $HOME/.merge0/pg, or the
#                   root-owned /var/lib/merge0-pg/data when run as root)
#   MERGE0_PG_PORT  port            (default: 55432)
#   MERGE0_PG_BIN   Postgres bin dir (default: pg_ctl on PATH, else the
#                   newest /usr/lib/postgresql/*/bin)

set -euo pipefail

PORT="${MERGE0_PG_PORT:-55432}"
SOCKET_DIR="/tmp"

# --- locate binaries ---------------------------------------------------
if [[ -n "${MERGE0_PG_BIN:-}" ]]; then
  BIN="$MERGE0_PG_BIN"
elif command -v pg_ctl >/dev/null 2>&1; then
  BIN="$(dirname "$(command -v pg_ctl)")"
else
  BIN="$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -n "${BIN:-}" && -x "$BIN/pg_ctl" ]] || {
  echo "error: no Postgres binaries found (install postgresql, or set MERGE0_PG_BIN)" >&2
  exit 1
}

# --- data dir + how to run pg commands ---------------------------------
# Postgres refuses to run as root; when invoked as root (containers, CI
# sandboxes) delegate to the `postgres` system user via su.
AS_PG=""
if [[ "$(id -u)" == "0" ]]; then
  id postgres >/dev/null 2>&1 || {
    echo "error: running as root but no 'postgres' user to delegate to" >&2
    exit 1
  }
  AS_PG="postgres"
  DATA="${MERGE0_PG_DATA:-/var/lib/merge0-pg/data}"
else
  DATA="${MERGE0_PG_DATA:-$HOME/.merge0/pg}"
fi

pg() { # run a postgres binary as the right user
  local cmd="$1"; shift
  if [[ -n "$AS_PG" ]]; then
    su "$AS_PG" -s /bin/bash -c "$(printf '%q ' "$BIN/$cmd" "$@")"
  else
    "$BIN/$cmd" "$@"
  fi
}

URL="postgres://merge0@localhost:$PORT/merge0"

init() {
  if [[ -f "$DATA/PG_VERSION" ]]; then
    echo "already initialized: $DATA"
    return 0
  fi
  mkdir -p "$DATA"
  # the delegated user needs the dir itself plus traversal of its parents
  [[ -n "$AS_PG" ]] && chown "$AS_PG" "$DATA"
  # Trust auth is deliberate: this is a loopback-only throwaway dev
  # cluster holding invented fixture data — never production config.
  pg initdb -D "$DATA" -U merge0 -A trust >/dev/null
  echo "initialized $DATA (superuser: merge0, auth: trust, dev-only)"
}

start() {
  init
  if pg pg_ctl -D "$DATA" status >/dev/null 2>&1; then
    echo "already running"
  else
    pg pg_ctl -D "$DATA" -l "$DATA/log" \
      -o "-p $PORT -k $SOCKET_DIR -c listen_addresses=localhost" \
      start >/dev/null
  fi
  # the tests default to database `merge0`; create it if missing
  if ! pg psql -h "$SOCKET_DIR" -p "$PORT" -U merge0 -d merge0 -c 'select 1' >/dev/null 2>&1; then
    pg createdb -h "$SOCKET_DIR" -p "$PORT" -U merge0 merge0
  fi
  echo "PG_OK $URL"
}

stop() {
  if pg pg_ctl -D "$DATA" status >/dev/null 2>&1; then
    pg pg_ctl -D "$DATA" -m fast stop >/dev/null
    echo "stopped"
  else
    echo "not running"
  fi
}

status() {
  if pg pg_ctl -D "$DATA" status >/dev/null 2>&1; then
    echo "PG_OK $URL"
  else
    echo "not running (data: $DATA)"
    return 1
  fi
}

destroy() {
  stop || true
  rm -rf "$DATA"
  echo "removed $DATA"
}

case "${1:-}" in
  init) init ;;
  start) start ;;
  stop) stop ;;
  status) status ;;
  url) echo "$URL" ;;
  destroy) destroy ;;
  *) grep '^# Usage:' "$0" | sed 's/^# //'; exit 2 ;;
esac
