#!/usr/bin/env bash
# Manual end-to-end drive of the real merge0-server binary over HTTP.
#
# Prereqs: Postgres reachable (default postgres://merge0@localhost:55432/merge0),
# the server built. Runs with MERGE0_DEV_FAKES=1 (fake model + fake GitHub) so
# no external credentials are needed; everything else — adapters, store,
# triage, budgets, auth, webhooks, telemetry — is the real code path.
#
# Usage: scripts/e2e-manual.sh [database_url]

set -euo pipefail
DB_URL="${1:-postgres://merge0@localhost:55432/merge0}"
PORT=18080
BASE="http://127.0.0.1:$PORT"
API_TOKEN="e2e-api-token"
RUNNER_TOKEN="e2e-runner-token"
WEBHOOK_SECRET="e2e-hook-secret"
SLACK_SIGNING="e2e-slack-signing"
POSTHOG_WEBHOOK_TOKEN="e2e-posthog-token"
JIRA_WEBHOOK_TOKEN="e2e-jira-token"
TENANT="e2e_manual_$(date +%s)"
PASS=0; FAIL=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '   \033[32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '   \033[31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
check() { # check <description> <actual> <expected-substring>
  if [[ "$2" == *"$3"* ]]; then ok "$1"; else bad "$1 — wanted '$3' in: $2"; fi
}
auth() { curl -sf -H "authorization: Bearer $API_TOKEN" "$@"; }

say "Starting merge0-server (dev fakes) on :$PORT, tenant $TENANT"
MERGE0_DATABASE_URL="$DB_URL" \
MERGE0_TENANT="$TENANT" \
MERGE0_REPO="chalk/chalk" \
MERGE0_DEV_FAKES=1 \
MERGE0_API_TOKEN="$API_TOKEN" \
MERGE0_RUNNER_TOKEN="$RUNNER_TOKEN" \
MERGE0_GITHUB_WEBHOOK_SECRET="$WEBHOOK_SECRET" \
MERGE0_SLACK_SIGNING_SECRET="$SLACK_SIGNING" \
MERGE0_POSTHOG_WEBHOOK_TOKEN="$POSTHOG_WEBHOOK_TOKEN" \
MERGE0_JIRA_WEBHOOK_TOKEN="$JIRA_WEBHOOK_TOKEN" \
MERGE0_TRIAGE_INTERVAL_SECS=0 \
MERGE0_BIND="127.0.0.1:$PORT" \
./target/debug/merge0-server &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  curl -sf "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
check "healthz (DB-backed)" "$(curl -sf "$BASE/healthz")" "ok"

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

say "1. Auth: every product route is closed without the API token"
for route in "reports" "telemetry" "metrics" "safety" "onboarding"; do
  STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$route")
  check "GET /$route unauthenticated -> 401" "$STATUS" "401"
done
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/triage/run")
check "POST /triage/run unauthenticated -> 401" "$STATUS" "401"

say "2. Ingest: Sentry via envelope + PostHog via NATIVE vendor webhook"
SENTRY_RES=$(auth -X POST "$BASE/ingest/sentry" -H "content-type: application/json" -d @- <<EOF
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

STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/webhooks/posthog" \
  -H "x-merge0-webhook-token: wrong-token" -H "content-type: application/json" -d '{}')
check "posthog webhook with bad token -> 401" "$STATUS" "401"
POSTHOG_RES=$(curl -sf -X POST "$BASE/webhooks/posthog" \
  -H "x-merge0-webhook-token: $POSTHOG_WEBHOOK_TOKEN" \
  -H "content-type: application/json" -d @- <<EOF
{"issue": {
  "id": "ph-9001", "name": "TypeError",
  "description": "Cannot read properties of undefined (reading 'districtId')",
  "first_seen": "2026-08-06T05:00:00Z", "last_seen": "$NOW", "users": 21
}}
EOF
)
check "posthog native webhook inserted=1" "$POSTHOG_RES" '"inserted":1'

JIRA_RES=$(curl -sf -X POST "$BASE/webhooks/jira" \
  -H "x-merge0-webhook-token: $JIRA_WEBHOOK_TOKEN" \
  -H "content-type: application/json" -d @- <<JIRAEOF
{"webhookEvent": "jira:issue_created", "issue": {
  "key": "CHK-901",
  "fields": {"summary": "Attendance export empty for large districts",
    "priority": {"name": "High"},
    "status": {"statusCategory": {"key": "indeterminate"}},
    "created": "2026-08-07T08:00:00.000Z", "updated": "$NOW"}}}
JIRAEOF
)
check "jira native webhook inserted=1" "$JIRA_RES" '"inserted":1' 

say "3. Triage run: cross-source cluster + jira ticket -> gate -> two Work Orders"
TRIAGE_RES=$(auth -X POST "$BASE/triage/run")
check "two reports created (cluster + ticket)" "$TRIAGE_RES" '"reports_created":2'
check "two work orders" "$TRIAGE_RES" '"work_orders":2'

REPORT_ID=$(auth "$BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; rs=json.load(sys.stdin); print(next(r["id"] for r in rs if "districtId" in r["title"]))')
SIGNALS=$(auth "$BASE/reports/$REPORT_ID" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["report"]["signal_ids"]))')
check "report references BOTH signals (P0-3)" "$SIGNALS" "2"

say "4. Surfaces: SPA shell, onboarding bundle, safety"
check "SPA shell serves (auth happens client-side)" "$(curl -sf "$BASE/inbox")" '<div id="root">'
check "dashboard route serves the same shell" "$(curl -sf "$BASE/dashboard")" '<div id="root">'
ONBOARDING=$(auth "$BASE/onboarding")
check "onboarding ships the workflow" "$ONBOARDING" "merge0.yml"
check "onboarding lists required secrets" "$ONBOARDING" "secrets_to_configure"
check "safety satisfied (fake protected repo)" "$(auth "$BASE/safety")" '"satisfied":true'

say "5. Approve via SLACK INTERACTION (signed) -> dispatch"
PAYLOAD_JSON='{"actions":[{"action_id":"approve","value":"'"$REPORT_ID"'"}]}'
SLACK_BODY="payload=$(python3 -c 'import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=""))' "$PAYLOAD_JSON")"
TS=$(date +%s)
FORGED_STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/slack/interactions" \
  -H "content-type: application/x-www-form-urlencoded" \
  -H "x-slack-request-timestamp: $TS" -H "x-slack-signature: v0=deadbeef" \
  --data "$SLACK_BODY")
check "forged slack signature -> 401" "$FORGED_STATUS" "401"
SIG="v0=$(printf 'v0:%s:%s' "$TS" "$SLACK_BODY" | openssl dgst -sha256 -hmac "$SLACK_SIGNING" | awk '{print $2}')"
APPROVE_RES=$(curl -sf -X POST "$BASE/slack/interactions" \
  -H "content-type: application/x-www-form-urlencoded" \
  -H "x-slack-request-timestamp: $TS" -H "x-slack-signature: $SIG" \
  --data "$SLACK_BODY")
check "slack Approve dispatched" "$APPROVE_RES" '"dispatched_to":"chalk/chalk"'

say "6. Runner callback: test-passing PR within diff budget; retry is a no-op"
CALLBACK_BODY=$(cat <<EOF
{"report_id": "$REPORT_ID", "status": "opened",
 "pr_url": "https://github.com/chalk/chalk/pull/77", "branch": "merge0/fix-$REPORT_ID",
 "tokens_spent": 110000, "files_changed": 2, "total_lines_changed": 38}
EOF
)
CB_RES=$(curl -sf -X POST "$BASE/runner/callback" \
  -H "authorization: Bearer $RUNNER_TOKEN" -H "content-type: application/json" \
  -d "$CALLBACK_BODY")
check "callback recorded as opened" "$CB_RES" '"status":"opened"'
CB_RETRY=$(curl -sf -X POST "$BASE/runner/callback" \
  -H "authorization: Bearer $RUNNER_TOKEN" -H "content-type: application/json" \
  -d "$CALLBACK_BODY")
check "retried callback -> duplicate no-op" "$CB_RETRY" '"duplicate":true'

say "7. GitHub webhook: PR merged (signed); redelivery + replay stay idempotent"
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
  -H "x-github-delivery: e2e-delivery-1" \
  -H "content-type: application/json" -d "$WEBHOOK_BODY")
check "merged outcome recorded" "$MERGE_RES" "merged outcome"
REDELIVERY=$(curl -sf -X POST "$BASE/webhooks/github" \
  -H "x-github-event: pull_request" -H "x-hub-signature-256: $SIG" \
  -H "x-github-delivery: e2e-delivery-1" \
  -H "content-type: application/json" -d "$WEBHOOK_BODY")
check "same delivery id -> short-circuited" "$REDELIVERY" '"duplicate_delivery"'
REPLAY=$(curl -sf -X POST "$BASE/webhooks/github" \
  -H "x-github-event: pull_request" -H "x-hub-signature-256: $SIG" \
  -H "x-github-delivery: e2e-delivery-2" \
  -H "content-type: application/json" -d "$WEBHOOK_BODY")
check "fresh delivery, same PR -> duplicate outcome ignored" "$REPLAY" "duplicate merged outcome"

say "8. Telemetry (P0-10) — counts unmoved by the replays"
TELEMETRY=$(auth "$BASE/telemetry?window_days=30")
check "1 dispatched" "$TELEMETRY" '"dispatched":1'
check "1 merged" "$TELEMETRY" '"prs_merged":1'
check "merge rate 100%" "$TELEMETRY" '"merge_rate":1.0'
check "cost accounting" "$TELEMETRY" '"tokens_per_merged_pr":110000.0'
METRICS=$(auth "$BASE/metrics")
check "prometheus metrics render" "$METRICS" "# TYPE merge0_prs_merged gauge"
check "prometheus merge count" "$METRICS" "merge0_prs_merged 1"

say "9. Revert detection: push reverting the merge -> hard negative"
PUSH_BODY=$(cat <<EOF
{"commits": [{"id": "ffff0000", "timestamp": "$MERGED_AT",
  "message": "Revert \"Fix the reported defect\"\n\nThis reverts commit e2efeedbeefcafe77."}]}
EOF
)
SIG="sha256=$(printf '%s' "$PUSH_BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | awk '{print $2}')"
REVERT_RES=$(curl -sf -X POST "$BASE/webhooks/github" \
  -H "x-github-event: push" -H "x-hub-signature-256: $SIG" \
  -H "x-github-delivery: e2e-delivery-3" \
  -H "content-type: application/json" -d "$PUSH_BODY")
check "revert recorded" "$REVERT_RES" "revert recorded"
check "telemetry counts the revert" "$(auth "$BASE/telemetry")" '"prs_reverted":1'

say "10. Signature enforcement on the GitHub webhook"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/webhooks/github" \
  -H "x-github-event: push" -H "x-hub-signature-256: sha256=deadbeef" -d '{}')
check "forged webhook rejected" "$STATUS" "401"

say "Result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
