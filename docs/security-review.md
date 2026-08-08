# Security review — pre-first-user pass (2026-08-08)

Scope: the three paths a first external user exposes to the internet or to
untrusted content — inbound webhooks, credential handling, and the dispatch
path that ends in an agent with write access to their repository. Plus the
cross-cutting questions those raise: route authentication coverage, SQL
injection surface, prompt injection from signal content, and SSRF in the
outbound fetchers.

This is a code review, not a penetration test, and it was performed against
a system that has never run against a live GitHub App or a production model
endpoint (see `README.md` and the caveat in `evals/BASELINE.md`). Findings
below were fixed in the same pass; each carries a regression test that was
verified to fail against the reintroduced bug.

## Findings

### 1. Zendesk webhook signatures never expired — MEDIUM, fixed

`verify_zendesk_signature` bound the request timestamp into the MAC but
never checked that the timestamp was recent. A captured Zendesk webhook
therefore authenticated forever: the signature scheme's entire replay
defence was being computed and then discarded. Slack's verifier in the same
codebase had always enforced a window; this one had not.

Impact was limited — replayed ingestion re-upserts the same fingerprint, so
there is no metric inflation — but "the MAC is valid" and "this request was
sent recently" are different claims, and the endpoint was making the first
while appearing to make the second.

**Fixed:** freshness is now checked *before* the MAC, with a ±5 minute
window (`ZENDESK_SIGNATURE_MAX_AGE_SECS`), matching the Slack verifier.
Regression test `a_perfectly_signed_zendesk_webhook_expires` asserts that a
correctly-signed request is refused once it ages out, including a
year-old capture and clock skew in both directions.

### 2. Two unauthenticated endpoints parsed the body before authenticating — LOW, fixed

`POST /webhooks/{vendor}` parsed the JSON body before running the vendor's
signature check, and `POST /broker/credentials` parsed before the runner key
was validated. Both sit on the open, rate-limited surface behind a 5 MB body
cap, so the practical impact is bounded CPU an anonymous caller can spend —
not a bypass.

What made this worth fixing rather than noting: `broker.rs`'s module
documentation *stated* that "auth runs strictly before body parsing." A
documented invariant that the code does not hold is worse than no comment,
because the next reader budgets their attention on the strength of it.

**Fixed:** vendor authentication moved into `verify_vendor`, called before
parsing (an unknown vendor now 404s without parsing at all), and the broker
authenticates via the new `Broker::authenticates` before touching the body.
Regression test `unauthenticated_callers_are_rejected_before_the_body_is_parsed`
sends malformed JSON with bad credentials to all four self-authenticating
endpoints and requires 401/404, not 400 — verified to fail (400) against the
reintroduced ordering.

### 3. No explicit redirect policy on outbound HTTP clients — LOW, not changed

Every outbound client (vendor pollers, GitHub, model, Slack, tracker) uses
`reqwest`'s default redirect policy and sends credentials. No confirmed
vulnerability: URLs are built from operator-configured base URLs, pagination
is carried as query parameters rather than followed as vendor-supplied URLs
(checked specifically — Sentry's `Link` header is parsed for its `cursor`
value only), and reqwest drops sensitive headers across hosts.

Recommended hardening rather than a fix, because it removes a question
instead of closing a hole: set a restrictive policy so a redirect can never
carry a credential somewhere unintended, and cannot reach an internal
address. Deferred deliberately — it is a change to every client's
construction and belongs with the first real-credential run, not ahead of
it.

## Verified, no finding

- **Route authentication coverage.** Every data route sits behind
  router-level bearer middleware. The exceptions are deliberate and each
  self-authenticates: GitHub webhook HMAC, runner callback token, Slack
  request signature, broker runner key. The two genuinely public routes
  (`/healthz` and the SPA shell) carry no data — the UI fetches everything
  client-side with the token.
- **Constant-time comparison** on every secret and MAC comparison found:
  API bearer, runner token, runner keys, and all vendor schemes.
- **Secrets are unprintable by construction.** `SecretToken` and `Secret`
  both render `[REDACTED]` for `Debug` *and* `Display`, exposure is behind
  named single-purpose accessors, and config references secrets by env-var
  name only. No logging statement was found that interpolates a credential.
- **Webhook secrets fail closed.** An unconfigured secret produces 503, not
  an unverified accept.
- **SQL injection.** The only interpolated identifier is the tenant schema
  name, validated against `[a-z_][a-z0-9_]*` (≤63 chars) at the single
  boundary where a `TenantStore` is constructed. Everything else is bound
  parameters.
- **The dispatch path does not become CI script injection.** This is the
  highest-severity plausible finding in the product's shape: work-order text
  is model-generated from untrusted signal content and executes inside the
  customer's GitHub Actions runner. It is handled correctly — untrusted
  values reach the workflow through `env:` (`toJson(...)`) and are read back
  with `jq -r` from a file, always quoted. No `${{ }}` expression containing
  untrusted content is interpolated into a `run:` body, which is the
  documented-dangerous pattern.
- **Work-order sanitization** rejects raw vendor payloads and credential
  markers before dispatch, and the gate prompt forbids copying secret-shaped
  values, exploit payloads, or exfiltration endpoints into a Work Order —
  with eval canaries that fail the corpus if it does.
- **Skill-registry path traversal.** `POST /registry/skills/{name}/install`
  resolves `{name}` against the *signed* index before building any
  filesystem path, so a traversal name cannot reach the disk without the
  signing key. Package content is verified against the listing's pinned
  hash.
- **Prompt injection.** Signal content is untrusted by design; the gate
  prompt states that signal text is data and never instructions, and the
  corpus carries an adversarial-injection scenario that must SKIP.
- **Rate limiting** is on by default (10 req/s per IP, burst 30) on the open
  surface, so signature verification is not a free DoS vector.

## Known gaps this review does not cover

- No live-credential run has happened: GitHub App token refresh, real
  installation scoping, and the production model client's error paths are
  untested outside wiremock.
- No `SECURITY.md` / disclosure contact exists yet. Add one with a real
  address before publishing the repository.
- Dependency and container-image vulnerability scanning is not wired into
  CI.
- Multi-tenant isolation in `ee/` was reviewed only where it touches the
  paths above; the hosted surface deserves its own pass before it serves
  more than one customer.
