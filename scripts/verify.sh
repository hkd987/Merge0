#!/usr/bin/env bash
# Run the repo's merge gates in CI order, with an explicit positive marker
# per gate. A gate has passed when its marker printed — absence of error
# output is not a pass (decision 12, docs/decision-log.md).
#
# Usage: scripts/verify.sh [options]
#   -p <crate>   scope clippy+test to a crate (repeatable). Use when the
#                tree carries someone else's in-flight work; run unscoped
#                before shipping.
#   --check      CI parity: fmt --check (report, don't rewrite files)
#   --skip-ui    skip the ui/ npm test + build gate
#   --skip-db    skip workspace tests that need Postgres is NOT possible —
#                this flag skips the *whole test gate* (fmt+clippy only).
#
# Postgres: tests expect postgres://merge0@localhost:55432/merge0 (or
# MERGE0_TEST_DATABASE_URL). `scripts/dev-pg.sh start` provides it.

set -euo pipefail
cd "$(dirname "$0")/.."

SCOPES=()
FMT_MODE="apply"
RUN_UI=1
RUN_TESTS=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    -p) SCOPES+=("$2"); shift 2 ;;
    --check) FMT_MODE="check"; shift ;;
    --skip-ui) RUN_UI=0; shift ;;
    --skip-db) RUN_TESTS=0; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

MARKERS=()
gate() { MARKERS+=("$1"); printf '\n\033[1;32mGATE %s: OK\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mGATE %s: FAILED\033[0m\n' "$1" >&2; exit 1; }

scope_args() { # clippy/test scoping
  if [[ ${#SCOPES[@]} -eq 0 ]]; then
    echo "--workspace"
  else
    printf ' -p %s' "${SCOPES[@]}"
  fi
}

# --- 1. fmt ------------------------------------------------------------
if [[ "$FMT_MODE" == "check" ]]; then
  cargo fmt --all --check || fail "fmt (diffs above are UNAPPLIED — run cargo fmt --all)"
else
  cargo fmt --all || fail "fmt"
  if ! git diff --quiet -- '*.rs'; then
    echo "note: fmt rewrote files (now formatted; review the diff)"
  fi
fi
gate "fmt"

# --- 2. clippy ---------------------------------------------------------
# shellcheck disable=SC2046
cargo clippy $(scope_args) --all-targets -- -D warnings || fail "clippy"
gate "clippy"

# --- 3. tests ----------------------------------------------------------
if [[ "$RUN_TESTS" == 1 ]]; then
  DB_URL="${MERGE0_TEST_DATABASE_URL:-postgres://merge0@localhost:55432/merge0}"
  if ! command -v psql >/dev/null 2>&1 || ! psql "$DB_URL" -c 'select 1' >/dev/null 2>&1; then
    # root sandboxes: psql may need the postgres user / socket — trust
    # dev-pg's own status check before giving up
    if ! scripts/dev-pg.sh status >/dev/null 2>&1; then
      echo "error: Postgres not reachable at $DB_URL" >&2
      echo "       start it with: scripts/dev-pg.sh start" >&2
      fail "test (precondition)"
    fi
  fi
  # shellcheck disable=SC2046
  cargo test $(scope_args) || fail "test"
  gate "test"
else
  echo "note: test gate SKIPPED (--skip-db) — do not ship on this run"
fi

# --- 4. ui -------------------------------------------------------------
if [[ "$RUN_UI" == 1 ]]; then
  if [[ ! -d ui/node_modules ]]; then
    echo "ui/node_modules missing — running npm ci"
    (cd ui && npm ci --silent) || fail "ui (npm ci)"
  fi
  (cd ui && npm test -- --run && npm run build) || fail "ui"
  gate "ui"
else
  echo "note: ui gate SKIPPED (--skip-ui)"
fi

# --- summary -----------------------------------------------------------
printf '\n\033[1mGates passed:%s\033[0m\n' "$(printf ' [%s]' "${MARKERS[@]}")"
if [[ "$RUN_TESTS" == 1 && "$RUN_UI" == 1 && ${#SCOPES[@]} -eq 0 ]]; then
  printf '\033[1;32mALL GATES GREEN (full workspace)\033[0m\n'
else
  printf '\033[1;33mPARTIAL RUN — rerun unscoped with no --skip flags before shipping\033[0m\n'
fi
