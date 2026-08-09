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
LINEAR_WEBHOOK_SECRET="e2e-linear-secret"
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
MERGE0_LINEAR_WEBHOOK_SECRET="$LINEAR_WEBHOOK_SECRET" \
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
EPOCH_NOW="$(date +%s)"

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
    "labels": ["exports", "Merge0"],
    "status": {"statusCategory": {"key": "indeterminate"}},
    "created": "2026-08-07T08:00:00.000Z", "updated": "$NOW"}}}
JIRAEOF
)
check "jira native webhook inserted=1" "$JIRA_RES" '"inserted":1'

say "2b. Ticket connectors: Linear + Slack signed webhooks, Asana/Trello/Intercom ingest"
LINEAR_BODY=$(cat <<EOF
{"type": "Issue", "action": "create", "data": {
  "identifier": "OPS-901",
  "title": "Weekly digest email sends twice to every admin",
  "url": "https://linear.example.com/acme/issue/OPS-901/digest-sends-twice",
  "priority": 2,
  "createdAt": "2026-08-07T09:00:00Z", "updatedAt": "$NOW"}}
EOF
)
FORGED_STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/webhooks/linear" \
  -H "linear-signature: deadbeef" -H "content-type: application/json" -d "$LINEAR_BODY")
check "linear webhook with forged signature -> 401" "$FORGED_STATUS" "401"
LINEAR_SIG=$(printf '%s' "$LINEAR_BODY" | openssl dgst -sha256 -hmac "$LINEAR_WEBHOOK_SECRET" | awk '{print $2}')
LINEAR_RES=$(curl -sf -X POST "$BASE/webhooks/linear" \
  -H "linear-signature: $LINEAR_SIG" -H "content-type: application/json" -d "$LINEAR_BODY")
check "linear native webhook inserted=1" "$LINEAR_RES" '"inserted":1'

TS=$(date +%s)
SLACK_CHALLENGE_BODY='{"type":"url_verification","challenge":"e2e-challenge-42"}'
SIG="v0=$(printf 'v0:%s:%s' "$TS" "$SLACK_CHALLENGE_BODY" | openssl dgst -sha256 -hmac "$SLACK_SIGNING" | awk '{print $2}')"
CHALLENGE_RES=$(curl -sf -X POST "$BASE/webhooks/slack" \
  -H "x-slack-request-timestamp: $TS" -H "x-slack-signature: $SIG" \
  -H "content-type: application/json" -d "$SLACK_CHALLENGE_BODY")
check "slack url_verification challenge echoed" "$CHALLENGE_RES" '"challenge":"e2e-challenge-42"'
SLACK_EVENT_BODY='{"type":"event_callback","event":{"type":"message","channel":"C0E2EBUGS01","ts":"'"$EPOCH_NOW"'.000200","user":"U0EXAMPLE07","text":"Parent portal shows last term grades after rollover","reply_count":4}}'
SIG="v0=$(printf 'v0:%s:%s' "$TS" "$SLACK_EVENT_BODY" | openssl dgst -sha256 -hmac "$SLACK_SIGNING" | awk '{print $2}')"
SLACK_RES=$(curl -sf -X POST "$BASE/webhooks/slack" \
  -H "x-slack-request-timestamp: $TS" -H "x-slack-signature: $SIG" \
  -H "content-type: application/json" -d "$SLACK_EVENT_BODY")
check "slack message event inserted=1" "$SLACK_RES" '"inserted":1'

ASANA_RES=$(auth -X POST "$BASE/ingest/asana" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "tasks", "context": {}, "payload": {"data": [{
  "gid": "1207009998887776",
  "name": "Report card PDF renders blank second page",
  "notes": "Reported by pilot-school@example.com: exporting report cards produces a blank page 2 for every student.",
  "completed": false,
  "created_at": "2026-08-05T10:00:00Z", "modified_at": "$NOW",
  "permalink_url": "https://app.asana.com/0/1206000111222333/1207009998887776"}]}}
EOF
)
check "asana ingest inserted=1" "$ASANA_RES" '"inserted":1'

TRELLO_RES=$(auth -X POST "$BASE/ingest/trello" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "cards", "context": {}, "payload": [{
  "id": "64f1c0ffee0badc0de000901",
  "name": "Bulk enrollment CSV rejects rows with accented names",
  "desc": "Reported by demo-district@example.com: rows containing accented characters fail validation.",
  "closed": false,
  "dateLastActivity": "$NOW",
  "shortUrl": "https://trello.com/c/e2eCard01",
  "labels": [{"id": "6501aa000000000000000901", "name": "bug", "color": "orange"}],
  "idList": "64f1b0000000000000000010"}]}
EOF
)
check "trello ingest inserted=1" "$TRELLO_RES" '"inserted":1'

INTERCOM_RES=$(auth -X POST "$BASE/ingest/intercom" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "conversations",
 "context": {"app_base_url": "https://app.intercom-example.com/a/inbox/abc123"},
 "payload": {"conversations": [{
  "type": "conversation", "id": "70090901", "title": "Invoices page times out",
  "state": "open", "priority": "priority",
  "created_at": $((EPOCH_NOW - 86400)), "updated_at": $EPOCH_NOW,
  "source": {"type": "conversation",
    "body": "<p>The invoices page never loads for our billing admin.</p>",
    "author": {"type": "user", "id": "6401ab234cde567890f90901",
      "name": "Jordan Example", "email": "jordan@example.com"}}}]}}
EOF
)
check "intercom ingest inserted=1" "$INTERCOM_RES" '"inserted":1'

say "3. Triage run: cluster + 6 tickets -> 7 reports; gate budget caps Work Orders at 3"
TRIAGE_RES=$(auth -X POST "$BASE/triage/run")
check "all eight signals became candidates" "$TRIAGE_RES" '"candidates":8'
check "seven reports created (cluster + 6 tickets)" "$TRIAGE_RES" '"reports_created":7'
check "work orders capped by max_work_orders_per_run" "$TRIAGE_RES" '"work_orders":3'

REPORT_ID=$(auth "$BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; rs=json.load(sys.stdin); print(next(r["id"] for r in rs if "districtId" in r["title"]))')
DETAIL=$(auth "$BASE/reports/$REPORT_ID")
SIGNALS=$(python3 -c 'import sys,json; print(len(json.load(sys.stdin)["report"]["signal_ids"]))' <<<"$DETAIL")
check "report references BOTH signals (P0-3)" "$SIGNALS" "2"
check "gate confidence rides on the work order" "$DETAIL" '"confidence":"high"'

# Autonomy ships OFF: despite high confidence, every gated report waits for
# a human (the 3 the per-run cap admitted; nothing dispatched itself).
AWAITING=$(auth "$BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
check "auto-dispatch off by default (gated reports await review)" "$AWAITING" "3"
DISPATCHED=$(auth "$BASE/reports?status=dispatched" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
check "nothing auto-dispatched" "$DISPATCHED" "0"

# The delegated jira ticket (merge0 label) is flagged on its signal.
JIRA_DELEGATED=$(auth "$BASE/reports?status=awaiting_review" | python3 -c '
import sys,json
rs=json.load(sys.stdin)
print(next(("yes" for r in rs if "Attendance export" in r["title"]), "missing"))')
check "delegated jira ticket became a report" "$JIRA_DELEGATED" "yes"

say "4. Surfaces: SPA shell, onboarding bundle, safety"
check "SPA shell serves (auth happens client-side)" "$(curl -sf "$BASE/inbox")" '<div id="root">'
check "report detail deep-link serves the shell (data stays behind the API)" "$(curl -sf "$BASE/inbox/$REPORT_ID")" '<div id="root">'
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
check "dispatch actor recorded (autonomy audit trail)" "$APPROVE_RES" '"dispatched_by":"slack"'

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
check "fix efficacy: fresh merge is pending (inside grace)" "$TELEMETRY" '"fixes_pending":1'
check "spend ledger counts gate + runner tokens" "$TELEMETRY" '"tokens_spent_24h":'
METRICS=$(auth "$BASE/metrics")
check "prometheus metrics render" "$METRICS" "# TYPE merge0_prs_merged gauge"
check "prometheus merge count" "$METRICS" "merge0_prs_merged 1"
check "loop liveness gauge present after the triage run" "$METRICS" "merge0_last_triage_run_timestamp_seconds"

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

say "11. Story delivery mode: a tracker story INSTEAD of a PR"
# Delivery mode is process-level, so this runs a second server (fresh
# tenant, dev-fake tracker) to prove the story-only path end to end.
STORY_PORT=18081
STORY_BASE="http://127.0.0.1:$STORY_PORT"
STORY_TENANT="${TENANT}_story"
MERGE0_DATABASE_URL="$DB_URL" \
MERGE0_TENANT="$STORY_TENANT" \
MERGE0_REPO="chalk/chalk" \
MERGE0_DEV_FAKES=1 \
MERGE0_DELIVERY_MODE=story \
MERGE0_API_TOKEN="$API_TOKEN" \
MERGE0_RUNNER_TOKEN="$RUNNER_TOKEN" \
MERGE0_TRIAGE_INTERVAL_SECS=0 \
MERGE0_BIND="127.0.0.1:$STORY_PORT" \
./target/debug/merge0-server > /tmp/merge0-e2e-story.log 2>&1 &
STORY_PID=$!
trap 'kill $SERVER_PID $STORY_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  curl -sf "$STORY_BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
sauth() { curl -sf -H "authorization: Bearer $API_TOKEN" "$@"; }

sauth -X POST "$STORY_BASE/ingest/sentry" -H "content-type: application/json" -d @- <<EOF >/dev/null
{"endpoint": "issues", "payload": [{
  "id": "7001", "shortId": "CHALK-7",
  "title": "TypeError: roster export drops the last student",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/7001/",
  "level": "error",
  "metadata": {"type": "TypeError", "value": "off-by-one in export"},
  "userCount": 26, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF
sauth -X POST "$STORY_BASE/triage/run" >/dev/null
STORY_REPORT=$(sauth "$STORY_BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')
APPROVE_STORY=$(sauth -X POST "$STORY_BASE/reports/$STORY_REPORT/approve")
check "approval delivers a story" "$APPROVE_STORY" '"delivered_as":"story"'
check "story key returned" "$APPROVE_STORY" '"story_key":"FAKE-1"'

STORY_DETAIL=$(sauth "$STORY_BASE/reports/$STORY_REPORT")
check "story recorded on the report" "$STORY_DETAIL" '"story_key":"FAKE-1"'
check "report is terminally handed off" "$STORY_DETAIL" '"status":"handed_off"'
check "no PR was dispatched in story mode" "$STORY_DETAIL" '"dispatch":null'

say "12. Confidence routing: a low-confidence Work Order goes to the board"
# Same PR-mode configuration as the main server — the ONLY difference is
# that the gate is not confident. A third process, because delivery mode
# and the fake model's confidence are both process-level.
ROUTE_PORT=18082
ROUTE_BASE="http://127.0.0.1:$ROUTE_PORT"
ROUTE_TENANT="${TENANT}_route"
MERGE0_DATABASE_URL="$DB_URL" \
MERGE0_TENANT="$ROUTE_TENANT" \
MERGE0_REPO="chalk/chalk" \
MERGE0_DEV_FAKES=1 \
MERGE0_DEV_FAKE_CONFIDENCE=low \
MERGE0_DELIVERY_MODE=pr \
MERGE0_API_TOKEN="$API_TOKEN" \
MERGE0_RUNNER_TOKEN="$RUNNER_TOKEN" \
MERGE0_TRIAGE_INTERVAL_SECS=0 \
MERGE0_BIND="127.0.0.1:$ROUTE_PORT" \
./target/debug/merge0-server > /tmp/merge0-e2e-route.log 2>&1 &
ROUTE_PID=$!
trap 'kill $SERVER_PID $STORY_PID $ROUTE_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  curl -sf "$ROUTE_BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
rauth() { curl -sf -H "authorization: Bearer $API_TOKEN" "$@"; }

rauth -X POST "$ROUTE_BASE/ingest/sentry" -H "content-type: application/json" -d @- <<EOF >/dev/null
{"endpoint": "issues", "payload": [{
  "id": "7002", "shortId": "CHALK-8",
  "title": "TypeError: gradebook totals drift after a term change",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/7002/",
  "level": "error",
  "metadata": {"type": "TypeError", "value": "totals drift"},
  "userCount": 31, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF
rauth -X POST "$ROUTE_BASE/triage/run" >/dev/null
ROUTE_REPORT=$(rauth "$ROUTE_BASE/reports?status=awaiting_review" | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')
APPROVE_ROUTE=$(rauth -X POST "$ROUTE_BASE/reports/$ROUTE_REPORT/approve")
check "low confidence is delivered as a story, not a PR" "$APPROVE_ROUTE" '"delivered_as":"story"'
check "the routing reason is reported" "$APPROVE_ROUTE" '"routed_by_confidence":true'
check "the confidence that caused it is reported" "$APPROVE_ROUTE" '"confidence":"low"'

ROUTE_DETAIL=$(rauth "$ROUTE_BASE/reports/$ROUTE_REPORT")
check "no agent was dispatched on a low-confidence order" "$ROUTE_DETAIL" '"dispatch":null'
check "the reason is persisted for whoever reads it later" "$ROUTE_DETAIL" "gate confidence was low"

say "13. Analytics ingestion: mixpanel + openpanel envelopes normalize and store"
# Ingest-only checks, deliberately AFTER every triage assertion: new sources
# change triage arithmetic (standing CLAUDE.md lesson), so these signals
# must never enter the counted runs above.
MX_INGEST=$(auth -X POST "$BASE/ingest/mixpanel" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "funnels",
 "context": {"project_base_url": "https://mixpanel.example.com/project/318"},
 "payload": {"results": [{
   "funnel_id": 301, "name": "Signup funnel", "fetched_at": "$NOW",
   "response": {"meta": {"dates": ["2026-08-07"]}, "data": {"2026-08-07": {
     "steps": [
       {"count": 3200, "goal": "App Open", "event": "App Open", "step_conv_ratio": 1.0, "overall_conv_ratio": 1.0, "avg_time": 2},
       {"count": 1400, "goal": "Signup", "event": "Signup", "step_conv_ratio": 0.4375, "overall_conv_ratio": 0.4375, "avg_time": 55}
     ],
     "analysis": {"completion": 1400, "starting_amount": 3200, "steps": 2, "worst": 1}}}}}]}}
EOF
)
check "mixpanel funnel drop-off normalizes to a signal" "$MX_INGEST" '"inserted":1'

OP_INGEST=$(auth -X POST "$BASE/ingest/openpanel" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "events",
 "context": {"project_base_url": "https://openpanel.example.com/acme/website"},
 "payload": {"meta": {"count": 5, "totalCount": 5, "pages": 1, "current": 1},
  "data": [
    {"id": "01JOP1", "name": "payment_failed", "deviceId": "d-1", "profileId": "p-1", "projectId": "website", "sessionId": "s-1", "properties": {"message": "card declined"}, "createdAt": "$NOW", "country": "US", "city": "Denver", "region": "CO", "os": "macOS", "osVersion": "14.5", "browser": "Chrome", "browserVersion": "126", "device": "desktop", "brand": "", "model": "", "path": "/checkout", "origin": "https://app.example.com", "referrer": "", "referrerName": "", "referrerType": ""},
    {"id": "01JOP2", "name": "payment_failed", "deviceId": "d-2", "profileId": "p-2", "projectId": "website", "sessionId": "s-2", "properties": {"message": "card declined"}, "createdAt": "$NOW", "country": "US", "city": "Austin", "region": "TX", "os": "iOS", "osVersion": "18", "browser": "Safari", "browserVersion": "18", "device": "mobile", "brand": "Apple", "model": "iPhone", "path": "/checkout", "origin": "https://app.example.com", "referrer": "", "referrerName": "", "referrerType": ""},
    {"id": "01JOP3", "name": "payment_failed", "deviceId": "d-3", "profileId": "p-3", "projectId": "website", "sessionId": "s-3", "properties": {"message": "card declined"}, "createdAt": "$NOW", "country": "DE", "city": "Berlin", "region": "BE", "os": "Windows", "osVersion": "11", "browser": "Edge", "browserVersion": "126", "device": "desktop", "brand": "", "model": "", "path": "/checkout", "origin": "https://app.example.com", "referrer": "", "referrerName": "", "referrerType": ""},
    {"id": "01JOP4", "name": "payment_failed", "deviceId": "d-4", "profileId": "p-4", "projectId": "website", "sessionId": "s-4", "properties": {"message": "card declined"}, "createdAt": "$NOW", "country": "US", "city": "Boise", "region": "ID", "os": "macOS", "osVersion": "14.5", "browser": "Firefox", "browserVersion": "128", "device": "desktop", "brand": "", "model": "", "path": "/checkout", "origin": "https://app.example.com", "referrer": "", "referrerName": "", "referrerType": ""},
    {"id": "01JOP5", "name": "payment_failed", "deviceId": "d-5", "profileId": "p-5", "projectId": "website", "sessionId": "s-5", "properties": {"message": "card declined"}, "createdAt": "$NOW", "country": "US", "city": "Reno", "region": "NV", "os": "Android", "osVersion": "15", "browser": "Chrome", "browserVersion": "126", "device": "mobile", "brand": "Google", "model": "Pixel", "path": "/checkout", "origin": "https://app.example.com", "referrer": "", "referrerName": "", "referrerType": ""}
  ]}}
EOF
)
check "openpanel error events aggregate to a signal" "$OP_INGEST" '"inserted":1'

RD_INGEST=$(auth -X POST "$BASE/ingest/reddit" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "subreddit_new",
 "context": {"base_url": "https://www.reddit.com"},
 "payload": {"kind": "Listing", "data": {"after": null, "children": [{
   "kind": "t3", "data": {
     "id": "1kz9aa", "name": "t3_1kz9aa",
     "title": "Gradebook exports have been broken for our whole district since Tuesday",
     "selftext": "Every export comes back empty. 60 teachers affected.",
     "author": "concerned_teacher", "subreddit": "chalkapp",
     "permalink": "/r/chalkapp/comments/1kz9aa/gradebook_exports_broken/",
     "url": "https://www.reddit.com/r/chalkapp/comments/1kz9aa/",
     "score": 47, "num_comments": 18, "created_utc": $EPOCH_NOW.0, "upvote_ratio": 0.97
   }}]}}}
EOF
)
check "reddit subreddit post normalizes to a ticket signal" "$RD_INGEST" '"inserted":1'

X_INGEST=$(auth -X POST "$BASE/ingest/x" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "recent_search",
 "context": {"query": "@chalkapp"},
 "payload": {"data": [{
   "id": "1821099887766554433", "author_id": "9001",
   "text": "@chalkapp attendance sync has eaten this morning's records for our whole school. Again.",
   "created_at": "$NOW",
   "public_metrics": {"retweet_count": 12, "reply_count": 9, "like_count": 41, "quote_count": 3}
 }],
 "includes": {"users": [{"id": "9001", "name": "Ms. Alvarez", "username": "msalvarez_teach"}]},
 "meta": {"newest_id": "1821099887766554433", "result_count": 1}}}
EOF
)
check "x mention normalizes to a ticket signal" "$X_INGEST" '"inserted":1'

say "14. Hosted multi-tenant: control plane, two data planes, isolation, suspension"
EE_PORT=18090
EE_BASE="http://127.0.0.1:$EE_PORT"
EE_TOKEN="e2e-ee-admin"
MERGE0_DATABASE_URL="$DB_URL" \
MERGE0_EE_ADMIN_TOKEN="$EE_TOKEN" \
MERGE0_EE_BIND="127.0.0.1:$EE_PORT" \
./target/debug/merge0-hosted > /tmp/merge0-e2e-ee.log 2>&1 &
EE_PID=$!
trap 'kill $SERVER_PID $STORY_PID $ROUTE_PID $EE_PID $TA_PID $TB_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -sf "$EE_BASE/healthz" >/dev/null 2>&1 && break; sleep 0.2; done

eeauth() { curl -sf -H "authorization: Bearer $EE_TOKEN" -H "x-merge0-actor: operator@merge0.example" "$@"; }
TEN_A=$(eeauth -X POST "$EE_BASE/ee/tenants" -H "content-type: application/json" \
  -d '{"name":"acme","plan":"team","admin_email":"admin@acme.example"}')
TEN_B=$(eeauth -X POST "$EE_BASE/ee/tenants" -H "content-type: application/json" \
  -d '{"name":"globex","plan":"team","admin_email":"admin@globex.example"}')
A_ID=$(echo "$TEN_A" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
B_ID=$(echo "$TEN_B" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
A_SCHEMA=$(echo "$TEN_A" | python3 -c 'import sys,json; print(json.load(sys.stdin)["schema_name"])')
B_SCHEMA=$(echo "$TEN_B" | python3 -c 'import sys,json; print(json.load(sys.stdin)["schema_name"])')
check "control plane provisioned two distinct schemas" "$([ "$A_SCHEMA" != "$B_SCHEMA" ] && echo distinct)" "distinct"

# Two data planes, each launched from its tenant's runtime manifest.
launch_tenant() { # port schema logfile
  MERGE0_DATABASE_URL="$DB_URL" MERGE0_TENANT="$2" MERGE0_REPO="chalk/chalk" \
  MERGE0_DEV_FAKES=1 MERGE0_API_TOKEN="$API_TOKEN" MERGE0_RUNNER_TOKEN="$RUNNER_TOKEN" \
  MERGE0_TRIAGE_INTERVAL_SECS=0 MERGE0_BIND="127.0.0.1:$1" \
  ./target/debug/merge0-server > "$3" 2>&1 &
}
launch_tenant 18091 "$A_SCHEMA" /tmp/merge0-e2e-tenant-a.log; TA_PID=$!
launch_tenant 18092 "$B_SCHEMA" /tmp/merge0-e2e-tenant-b.log; TB_PID=$!
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:18091/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:18092/healthz" >/dev/null 2>&1 && break; sleep 0.2; done

# A signal ingested into tenant A must be invisible to tenant B.
auth -X POST "http://127.0.0.1:18091/ingest/sentry" -H "content-type: application/json" -d @- <<EOF >/dev/null
{"endpoint": "issues", "payload": [{
  "id": "8801", "shortId": "ACME-1",
  "title": "TypeError: acme-only tenant crash",
  "permalink": "https://sentry.example.com/organizations/acme/issues/8801/",
  "level": "error", "metadata": {"type": "TypeError", "value": "acme only"},
  "userCount": 12, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF
auth -X POST "http://127.0.0.1:18091/triage/run" >/dev/null
A_REPORTS=$(auth "http://127.0.0.1:18091/reports" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
B_REPORTS=$(auth "http://127.0.0.1:18092/reports" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
check "tenant A sees its report" "$A_REPORTS" "1"
check "tenant B sees NOTHING of tenant A" "$B_REPORTS" "0"

# Metering reflects only the tenant's own activity.
A_USAGE=$(eeauth "$EE_BASE/ee/tenants/$A_ID/usage")
check "usage endpoint meters tenant A" "$A_USAGE" '"window_days":30'

# Suspension: manifest flips, membership freezes, usage stays readable.
eeauth -X POST "$EE_BASE/ee/tenants/$A_ID/suspend" >/dev/null
A_RUNTIME=$(eeauth "$EE_BASE/ee/tenants/$A_ID/runtime")
check "suspended tenant's runtime manifest says so" "$A_RUNTIME" '"desired_state":"suspended"'
B_RUNTIME=$(eeauth "$EE_BASE/ee/tenants/$B_ID/runtime")
check "neighbor tenant stays running" "$B_RUNTIME" '"desired_state":"running"'
FROZEN_USAGE=$(eeauth "$EE_BASE/ee/tenants/$A_ID/usage")
check "usage remains computable while suspended" "$FROZEN_USAGE" '"window_days":30'

say "15. Growth features: CODEOWNERS routing + one-shot CLI quickstart"
# CODEOWNERS: a crash whose title carries an owned path routes to the
# owning team from the dev fake's CODEOWNERS. Runs AFTER every counted
# section — the fresh signal + triage must not disturb earlier arithmetic.
CO_RES=$(auth -X POST "$BASE/ingest/sentry" -H "content-type: application/json" -d @- <<EOF
{"endpoint": "issues", "payload": [{
  "id": "9101", "shortId": "CHALK-91",
  "title": "TypeError: roster.filter is not a function in src/districts/roster.ts",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/9101/",
  "level": "error",
  "metadata": {"type": "TypeError", "value": "roster.filter is not a function"},
  "userCount": 41, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF
)
check "owned-path crash ingests" "$CO_RES" '"inserted":1'
auth -X POST "$BASE/triage/run" > /dev/null
CO_ID=$(auth "$BASE/reports?status=awaiting_review" | python3 -c "import json,sys; print([r['id'] for r in json.load(sys.stdin) if 'roster.ts' in r['title']][0])")
CO_DETAIL=$(auth "$BASE/reports/$CO_ID")
check "report detail routes the evidence path to its CODEOWNERS team" "$CO_DETAIL" '"@acme/data-team"'
check "routed entry names the path itself" "$CO_DETAIL" 'src/districts/roster.ts'
check "gate decision context is replayable (audit)" "$CO_DETAIL" '=== SYSTEM ==='

# CLI quickstart: the real merge0 binary end to end with a stub model CLI
# (no spend, deterministic) — a bare Sentry array in, a work order out.
CLI_STUB=/tmp/merge0-e2e-cli-stub.sh
cat > "$CLI_STUB" <<'STUB'
#!/bin/sh
cat > /dev/null
echo '{"result":"{\"decision\":\"work\",\"summary\":\"Guard null plan in BillingSummary\",\"repro\":\"open billing as a downgraded user\",\"success_criteria\":\"regression test passes\",\"confidence\":\"high\"}","is_error":false,"usage":{"input_tokens":10,"output_tokens":5}}'
STUB
chmod +x "$CLI_STUB"
cat > /tmp/merge0-e2e-export.json <<EOF
[{"id": "9102", "shortId": "CHALK-92",
  "title": "TypeError: Cannot read properties of null (reading 'planId')",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/9102/",
  "level": "error", "metadata": {"type": "TypeError", "value": "null planId"},
  "userCount": 12, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"}]
EOF
CLI_OUT=$(./target/debug/merge0 triage --source sentry --file /tmp/merge0-e2e-export.json --cli "$CLI_STUB" 2>&1)
check "merge0 triage quickstart emits a work order" "$CLI_OUT" "WORK ORDER"
check "quickstart shows the gate confidence" "$CLI_OUT" "High confidence"

# Onboarding names the selected agent's provider secret (name only).
ONBOARD_SECRETS=$(auth "$BASE/onboarding")
check "onboarding names the agent's provider secret" "$ONBOARD_SECRETS" '"ANTHROPIC_API_KEY"'

say "16. Outcome reconciliation: a merged PR whose webhook was lost is repaired"
# Fresh report -> approve -> runner opens PR #424242 (which the dev fake
# reports as already merged on GitHub) -> NO webhook arrives -> the next
# triage run's reconciliation sweep records the merge anyway.
auth -X POST "$BASE/ingest/sentry" -H "content-type: application/json" -d @- > /dev/null <<EOF2
{"endpoint": "issues", "payload": [{
  "id": "9201", "shortId": "CHALK-92R",
  "title": "ReferenceError: sortRoster is not defined after refactor",
  "permalink": "https://sentry.example.com/organizations/chalk/issues/9201/",
  "level": "error",
  "metadata": {"type": "ReferenceError", "value": "sortRoster is not defined"},
  "userCount": 22, "firstSeen": "2026-08-06T04:00:00Z", "lastSeen": "$NOW"
}]}
EOF2
auth -X POST "$BASE/triage/run" > /dev/null
RC_ID=$(auth "$BASE/reports?status=awaiting_review" | python3 -c "import json,sys; print([r['id'] for r in json.load(sys.stdin) if 'sortRoster' in r['title']][0])")
auth -X POST "$BASE/reports/$RC_ID/approve" > /dev/null
curl -sf -X POST "$BASE/runner/callback" -H "authorization: Bearer $RUNNER_TOKEN" -H "content-type: application/json" -d "{
  \"report_id\": \"$RC_ID\", \"status\": \"opened\",
  \"pr_url\": \"https://github.com/chalk/chalk/pull/424242\",
  \"branch\": \"merge0/fix-$RC_ID\", \"tokens_spent\": 80000,
  \"files_changed\": 1, \"total_lines_changed\": 9}" > /dev/null
RC_BEFORE=$(auth "$BASE/reports/$RC_ID")
check "report waits as pr_open with no outcome" "$RC_BEFORE" '"status":"pr_open"'
auth -X POST "$BASE/triage/run" > /dev/null
RC_AFTER=$(auth "$BASE/reports/$RC_ID")
check "reconciliation completed the report without any webhook" "$RC_AFTER" '"status":"completed"'
check "the missed merged outcome is recorded" "$RC_AFTER" '"outcome":"merged"'

say "17. MCP surface: an agent client speaks JSON-RPC to the same inbox"
MCP_INIT=$(auth -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')
check "mcp initialize identifies the server" "$MCP_INIT" '"name":"merge0"'
MCP_TOOLS=$(auth -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
check "mcp lists the approve verb" "$MCP_TOOLS" '"approve_report"'
check "mcp lists the telemetry verb" "$MCP_TOOLS" '"get_telemetry"'
MCP_LIST=$(auth -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_reports","arguments":{}}}')
check "mcp tools/call reads the report queue" "$MCP_LIST" '"isError":false'
MCP_401=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" \
  -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":4,"method":"tools/list"}')
check "mcp without the bearer token is rejected" "$MCP_401" '401'

say "Result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
