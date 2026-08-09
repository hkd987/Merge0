# Decision log

Why the codebase is shaped the way it is. Contributors inherit these
decisions; this file records the *reasoning* so you can tell the
load-bearing walls from the wallpaper — and so a decision is relitigated
with its original evidence on the table, not from memory.

Companion reading: `CLAUDE.md` (the invariants themselves and the
self-updating lessons fence), `docs/PRD.md` (product intent),
`docs/security-review.md` (threat-model decisions). The bug half of this
history is in the table at the bottom: every fix that taught us something
is mapped to the artifact that now prevents its return.

## Decisions that shape the codebase

### 1. A human merges every change — autonomy is opt-in, off by default

The entire trust posture hangs on this. `auto_dispatch = false` ships in
`config/gate.toml`, an explicit test asserts the default, and the
confidence router (`route_by_confidence` in
`crates/merge0-server/src/handlers/actions.rs`) only ever *downgrades* a
delivery — an unconfident Work Order becomes a tracker story for a human,
never the reverse. When you touch autonomy code, preserve the direction:
Merge0 earns trust by being conservative when unsure, and every relaxation
must be something an operator explicitly turned on.

### 2. Adapters are quarantined; the Signal schema is a treaty

Vendor types never cross the adapter boundary (invariant 1), and any
change to `merge0-signal` is a spec change: `docs/signal-schema.md` bumps
its version in the same PR, and a round-trip test keeps the doc's JSON
example honest. The schema went v0.1 → v0.6 this way (reports/pipeline,
ticket sources, `delegated` + `GateConfidence`, `OutcomeRef.pr_url`,
Mixpanel/OpenPanel) without any consumer ever parsing a vendor payload.
The treaty is what makes "add a source" a one-crate change.

### 3. Model output is weather; parse it fail-conservative

A live eval — not a unit test — revealed that models emit lists where a
string was asked for, and our strict serde parser fail-closed every WORK
verdict to SKIP (gate accuracy: 64%). Two rules came out of it:
tolerant-parse benign shape variance (`ModelVerdict` fields accept
string-or-list), and when a *judgment* field is unparseable, fail toward
the conservative branch — absent/garbled confidence parses to `Low`, which
routes to a human. Never let a parse failure silently pick the permissive
branch.

### 4. Evals are measured, never asserted

Prompt and memory changes must show up as numbers: the corpus
(`evals/scenarios/`, 30 cases) runs against the real gate code with the
shipped prompt, the bar is exit-code-enforced (≥85% accuracy, zero canary
leaks), borderline scenarios are scored as *rates* over repeated samples
(`samples`/`min_pass_rate`), and behavior changes are A/B'd against the
reconstructed pre-change behavior rather than eyeballed. This discipline
killed a planned feature: temperature/self-consistency machinery was
rejected because nine consecutive correct verdicts left no variance to
reduce (`evals/BASELINE.md` run 7). It also carries a recorded caveat —
the eval backend is the Claude Code CLI while production is
`AnthropicModel`, so the corpus cannot validate production *sampling*
changes. Honest caveats in `BASELINE.md` are part of the method.

### 5. Prompts are config, not code

Scout queries and the gate prompt live in `config/`, not in Rust string
literals (invariant 6). The reason is the meta-loop: Merge0 proposes
changes to its own judgment as ordinary reviewable PRs, and that only
works if judgment is data. If you find yourself embedding prompt text in a
crate, you're on the wrong side of this line.

### 6. The hardening hierarchy: prevention is an artifact, not a memory

A bug fix is not done until something *executable* stops its recurrence
(invariant 9): a repo-hygiene rule or clippy lint beats a regression test
beats an eval canary beats a CLAUDE.md prose lesson. Two corollaries
learned the hard way: a new rule must be *mutation-verified* (reintroduce
the bug, watch the rule fail — one rule initially "passed" because a
dependency cycle broke the build before the rule ever ran), and a rule
must be precise (false positives train people to ignore the lint).
Recurring prose lessons get promoted into rules and then deleted from the
prose — an ever-growing lesson list is context rot.

### 7. Secrets are names, never values — and "secrets" includes exploits

Config, fixtures, control-plane manifests, and tests carry env-var
*names* only (invariant 4); fixtures use invented example.com data. The
same redaction discipline extends further than credentials: a live eval
caught the gate copying a working stored-XSS payload and its exfil
endpoint verbatim into a Work Order (which would have traveled into a PR
body and Slack). The gate prompt now forbids reproducing working exploit
payloads and attacker endpoints — while deliberately *allowing* ubiquitous
API identifiers like `document.cookie`, because a gate that redacts those
can't write a useful repro. Scenario 26 keeps this honest with a canary.

### 8. Auth before parse, everywhere

An axum `Json<T>` extractor runs before the handler body, so a malformed
payload got a 422 from an endpoint that should have said 401 first.
Authenticated endpoints take `Bytes` and parse *after* the auth check;
vendor webhooks verify their MAC (with a bounded replay window — Zendesk
was ±5min *before* the MAC check) before touching content. Regression
tests pin both orderings; the manual e2e sweeps every product route
unauthenticated. New endpoints must join the sweep.

### 9. Every configured source must be able to reach triage

The most instructive bug of the project: six sources (datadog, loopforge,
mixpanel, openpanel, otel, webhook) ingested perfectly and then vanished —
no shipped scout's query could ever select their `(source, kind)` pairs,
so their signals sat in the store forever. Nothing errored; the loop was
just silently incomplete, and only a *live* end-to-end run with fake
vendors exposed it. Now enforced two ways:
`every_adapter_source_is_selectable_by_at_least_one_shipped_scout` in
`crates/merge0-e2e/tests/repo_hygiene.rs` runs the real query engine over
the shipped scouts for every adapter golden, and pollers fail loudly at
construction when enabled but unconfigured (empty `funnel_ids` /
`error_events`), because a poller that silently polls nothing is the same
bug wearing a different hat.

### 10. Open-core with a hard, tested boundary

MIT `crates/` may never depend on `ee/` (invariant 5, enforced by
`no_mit_crate_depends_on_the_ee_directory`). Where ee features need a hook
in the core, the hook ships MIT-generic: `MERGE0_GATE_CONTEXT_EXTRA` is an
ordinary "deployment-provided context" env var that happens to be how
hosted cross-tenant priors reach a tenant's gate. The ee tree carries a
PostHog-pattern enterprise license; contributions require a one-comment
CLA (`CLA.md`, enforced by `.github/workflows/cla.yml`) because
contributed code lives on both sides of the boundary. That workflow runs
on `pull_request_target` and must NEVER check out PR code — the warning
comment in the file is load-bearing.

### 11. Hosted multi-tenancy: schema-per-tenant, process-per-tenant, and a core that doesn't know

The control plane (`ee/merge0-hosted`) owns tenant lifecycle; each tenant
runs a stock MIT `merge0-server` against its own Postgres schema. Nothing
multi-tenant executes in the data plane — which is why the core needs no
tenant logic and the hosted product can't leak across tenants in code the
tenants run. Two deliberate wrinkles: never point two data planes at one
schema (single-writer scheduler), and suspension freezes writes and
membership but *metering and audit deliberately keep working* — the
invoice that justified a suspension must remain computable, and the audit
trail must not go dark exactly when you'd want to read it.

### 12. Verification means seeing the positive marker

Process decision, encoded after being burned: `cargo fmt --all --check`
*reports* diffs, it never applies them, and "no error output" is not a
pass. A gate has passed when its explicit success marker printed
(`scripts/verify.sh` prints one per gate and refuses to summarize green
without all of them). Related: when another session or contributor has
in-flight changes in the tree, scope verification with `-p <crate>` and
run the `--all` forms only when the tree is yours.

## Bugs → the artifact that now prevents them

Selected incidents with a generalizable lesson. "Prevention" names the
executable artifact, per decision 6; `hygiene` means
`crates/merge0-e2e/tests/repo_hygiene.rs`.

| What broke | Lesson | Prevention |
|---|---|---|
| Six sources ingested but no scout could select them | The loop can be silently incomplete with zero errors | hygiene: scout-reachability rule; pollers fail loudly when enabled-but-unconfigured |
| Strict serde zeroed gate yield when the model emitted lists | Model output shape is weather | string-or-list parsing + live-eval canary (only a live model exposed it) |
| XSS payload + exfil endpoint copied into a Work Order | Redaction must cover exploits, not just credentials | gate-prompt rule + eval canary (scenario 26) |
| 422 before 401 on authenticated endpoints | Extractors parse before handlers run | `Bytes`-then-auth pattern + auth-sweep tests |
| Zendesk webhook replayable | Freshness must be checked before, and alongside, the MAC | ±5min window before MAC + regression test |
| `SUM()` overflowed / `Bearer ` prefix compared raw | Recurring one-liners deserve lints, not memory | hygiene: SQL-cast + bearer-prefix rules (promoted from CLAUDE.md prose) |
| An adapter could never clear the gate's severity floor | An adapter that can't surface work is dead code | hygiene: gate-floor emitability rule |
| utf8 eval fixture started green (truncation index landed on a char boundary) | A fixture that starts green measures nothing | harness baseline-sanity check + rule in `evals/README.md`: prove the fixture red first |
| Boundary-rule "passed" while a dep cycle broke the build first | An unverified rule is a placebo | mutation-verify every new rule against a non-cyclic crate |
| Scout-reachability rule passed vacuously (`"<ulid>"` placeholder silently skipped) | A silent `continue` can empty a check | rule panics loudly on unparseable goldens |
| `timestamptz` round-trip compared `<` its original | Postgres truncates to microseconds | tolerance/truncation in tests (CLAUDE.md lesson) |
| Claimed "gates green" off `fmt --check` diff output | A pass is a positive marker, not absent errors | `scripts/verify.sh` marker discipline (decision 12) |
| e2e counts broke when a scout's source list grew | New sources change triage arithmetic | e2e selects reports by content, never index (CLAUDE.md lesson) |

## Adding to this file

Log a *decision* here when it constrains future contributors and the
reason isn't obvious from the code. Log a *bug* here only after its
prevention artifact exists — the row's job is to point at the artifact.
One-off typos and anecdotes don't belong; that's what the CLAUDE.md
lessons fence and git history are for.
