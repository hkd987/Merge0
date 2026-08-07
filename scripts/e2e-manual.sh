#!/usr/bin/env bash
# Manual end-to-end drive of the real merge0-server binary over HTTP.
#
# Prereqs: Postgres reachable (default postgres://merge0@localhost:55432/merge0),
# the server built. Runs with MERGE0_DEV_FAKES=1 (fake model + fake GitHub) so
# no external credentials are needed; everything else — adapters, store,
# triage, budgets, webhooks, telemetry — is the real code path.
#
# Usage: scripts/e2e-manual.sh [database_url]

set -euo pipefail
DB_URL="${1:-postgres://merge0@localhost:55432/merge0}"
PORT=18080
BASE="http://127.0.0.1:$PORT"
API_TOKEN="e2e-api-token"
RUNNER_TOKEN="e2e-runner-token"
WEBHOOK_SECRET="e2e-hook-secret"
TENANT="e2e_manual_$(date +%s)"
PASS=0; FAIL=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '   \033[32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '   \033[31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
check() { # check <description> <actual> <expected-substring>
  if [[ "$2" == *"$3"* ]]; then ok "$1"; else bad "$1 — wanted '$3' in: $2"; fi
}

say "Starting merge0-server (dev fakes) on :$PORT, tenant $TENANT"
MERGE0_DATABASE_URL="$DB_URL" \
MERGE0_TENANT="$TENANT" \
MERGE0_REPO="chalk/chalk" \
MERGE0_DEV_FAKES=1 \
MERGE0_API_TOKEN="$API_TOKEN" \
MERGE0_RUNNER_TOKEN="$RUNNER_TOKEN" \
MERGE0_GITHUB_WEBHOOK_SECRET="$WEBHOOK_SECRET" \
MERGE0_TRIAGE_INTERVAL_SECS=0 \
MERGE0_BIND="127.0.0.1:$PORT" \
./target/debug/merge0-server &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  curl -sf "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
check "healthz" "$(curl -sf "$BASE/healthz")" "ok"

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

say "1. Ingest: Sentry issue + PostHog rage clicks (correlated by stack hash / path)"
SENTRY_RES=$(curl -sf -X POST "$BASE/ingest/sentry" \
  -H "authorization: Bearer $API_TOKEN" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "issues", "payload": [{
  "id": "9001", "shortId": "CHALK-9",
  "title": "TypeError: Cannot read properties of undefined (reading 'districtId')",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/9001/",
  "level": "error",
  "metadata": {"type": "TypeError", "value": "districtId undefined"},
  "userCount": 33, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF
)
check "sentry ingest inserted=1" "$SENTRY_RES" '"inserted":1'

POSTHOG_RES=$(curl -sf -X POST "$BASE/ingest/posthog" \
  -H "authorization: Bearer $API_TOKEN" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "error_tracking_issues",
 "context": {"project_base_url": "https://us.posthog.com/project/1"},
 "payload": {"results": [{
   "id": "ph-9001", "name": "TypeError",
   "description": "Cannot read properties of undefined (reading 'districtId')",
   "first_seen": "2026-08-06T05:00:00Z", "last_seen": "$NOW", "users": 21
}]}}
EOF
)
check "posthog ingest inserted=1" "$POSTHOG_RES" '"inserted":1'

say "2. Triage run: cross-source cluster -> gate -> one Work Order"
TRIAGE_RES=$(curl -sf -X POST "$BASE/triage/run" -H "authorization: Bearer $API_TOKEN")
check "one report created" "$TRIAGE_RES" '"reports_created":1'
check "one work order" "$TRIAGE_RES" '"work_orders":1'

REPORT_ID=$(curl -sf "$BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')
SIGNALS=$(curl -sf "$BASE/reports/$REPORT_ID" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["report"]["signal_ids"]))')
check "report references BOTH signals (P0-3)" "$SIGNALS" "2"

say "3. Inbox renders; safety verified; approve dispatches"
check "inbox HTML shows the report" "$(curl -sf "$BASE/inbox")" "TypeError"
check "safety satisfied (fake protected repo)" "$(curl -sf "$BASE/safety")" '"satisfied":true'
APPROVE_RES=$(curl -sf -X POST "$BASE/reports/$REPORT_ID/approve" -H "authorization: Bearer $API_TOKEN")
check "approved and dispatched" "$APPROVE_RES" '"dispatched_to":"chalk/chalk"'

say "4. Runner callback: test-passing PR within diff budget"
CB_RES=$(curl -sf -X POST "$BASE/runner/callback" \
  -H "authorization: Bearer $RUNNER_TOKEN" -H "content-type: application/json" -d @- <<EOF
{"report_id": "$REPORT_ID", "status": "opened",
 "pr_url": "https://github.com/chalk/chalk/pull/77", "branch": "merge0/fix-$REPORT_ID",
 "tokens_spent": 110000, "files_changed": 2, "total_lines_changed": 38}
EOF
)
check "callback recorded as opened" "$CB_RES" '"status":"opened"'

say "5. GitHub webhook: PR merged (signed)"
MERGED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
WEBHOOK_BODY=$(cat <<EOF
{"action": "closed", "pull_request": {
  "html_url": "https://github.com/chalk/chalk/pull/77", "merged": true,
  "merged_at": "$MERGED_AT", "merge_commit_sha": "e2efeedbeefcafe77",
  "title": "Fix the reported defect", "body": ""}}
EOF
)
SIG="sha256=$(printf '%s' "$WEBHOOK_BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | awk '{print $2}')"
MERGE_RES=$(curl -sf -X POST "$BASE/webhooks/github" \
  -H "x-github-event: pull_request" -H "x-hub-signature-256: $SIG" \
  -H "content-type: application/json" -d "$WEBHOOK_BODY")
check "merged outcome recorded" "$MERGE_RES" "merged outcome"

say "6. Telemetry (P0-10)"
TELEMETRY=$(curl -sf "$BASE/telemetry?window_days=30")
check "1 dispatched" "$TELEMETRY" '"dispatched":1'
check "1 merged" "$TELEMETRY" '"prs_merged":1'
check "merge rate 100%" "$TELEMETRY" '"merge_rate":1.0'
check "cost accounting" "$TELEMETRY" '"tokens_per_merged_pr":110000.0'

say "7. Revert detection: push reverting the merge -> hard negative"
PUSH_BODY=$(cat <<EOF
{"commits": [{"id": "ffff0000", "timestamp": "$MERGED_AT",
  "message": "Revert \"Fix the reported defect\"\n\nThis reverts commit e2efeedbeefcafe77."}]}
EOF
)
SIG="sha256=$(printf '%s' "$PUSH_BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | awk '{print $2}')"
REVERT_RES=$(curl -sf -X POST "$BASE/webhooks/github" \
  -H "x-github-event: push" -H "x-hub-signature-256: $SIG" \
  -H "content-type: application/json" -d "$PUSH_BODY")
check "revert recorded" "$REVERT_RES" "revert recorded"
check "telemetry counts the revert" "$(curl -sf "$BASE/telemetry")" '"prs_reverted":1'

say "8. Negative paths: auth + signature enforcement"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/triage/run")
check "unauthenticated triage rejected" "$STATUS" "401"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/webhooks/github" \
  -H "x-github-event: push" -H "x-hub-signature-256: sha256=deadbeef" -d '{}')
check "forged webhook rejected" "$STATUS" "401"

say "Result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
