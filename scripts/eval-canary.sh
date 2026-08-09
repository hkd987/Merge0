#!/usr/bin/env bash
# Weekly eval canary: the smallest live-model run that catches the failure
# classes deterministic tests cannot — redaction leaks and decision drift.
#
# Evals never run in CI (model spend + nondeterminism); this script is the
# operator-cron complement. It runs a FIXED five-scenario subset:
#
#   01-clear-crash            decision sanity: an obvious WORK stays WORK
#   04-intended-behavior      decision sanity: an obvious SKIP stays SKIP
#   10-secret-canary          credentials must not travel into the order
#   26-xss-poc                exploit payloads/exfil endpoints must not either
#   31-social-pii-redaction   reporter identities (handles, emails) must not
#
# Spend is capped by construction: five scenarios, ~10-15k tokens per run
# (versus ~40k for the full corpus). The subset is deliberately hardcoded —
# a canary you can quietly reconfigure is not a canary.
#
# Usage:
#   scripts/eval-canary.sh                 # uses the CLI default model
#   MERGE0_EVAL_MODEL=claude-sonnet-5 scripts/eval-canary.sh
#
# Cron example (weekly, Monday 06:00):
#   0 6 * * 1  cd /path/to/merge0 && scripts/eval-canary.sh >> /var/log/merge0-canary.log 2>&1
#
# Exit code is gate-eval's own bar: 0 only when decision accuracy >= 85%
# AND zero canary leaks. Wire the failure into whatever pages you.

set -euo pipefail
cd "$(dirname "$0")/.."

CANARY_SCENARIOS=(
  01-clear-crash
  04-intended-behavior
  10-secret-canary
  26-xss-poc
  31-social-pii-redaction
)

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/scenarios"
for scenario in "${CANARY_SCENARIOS[@]}"; do
  cp "evals/scenarios/$scenario.toml" "$WORKDIR/scenarios/"
done

echo "== Merge0 eval canary: ${#CANARY_SCENARIOS[@]} scenarios (live model) =="
MERGE0_EVAL_SCENARIOS="$WORKDIR/scenarios" \
  cargo run -q -p merge0-evals --bin gate-eval
echo "== Canary green: redaction holding, no decision drift =="
