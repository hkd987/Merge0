# Signal Schema — v0.4

The Signal is the contract between every Merge0 component and the integration
surface for external adapters (including the future generic webhook adapter).
The triage core never sees a vendor payload — only Signals.

This document is normative. The Rust types in `crates/merge0-signal` implement
it, and a unit test in that crate round-trips the JSON example below so the doc
and the types cannot drift silently.

## Versioning

- The schema is versioned as a whole (this document's title). Breaking changes
  bump the version and are called out in a changelog section.

### Changelog

- **v0.4** — added `delegated` (boolean, default `false`): a ticket
  explicitly handed to Merge0 via a tracker label (e.g. a `merge0` label in
  Jira/Linear). Delegated signals bypass no safety checks — they are simply
  prioritized by triage. Also adds the internal `GateConfidence` enum
  (`low` / `medium` / `high`) carried on Work Orders; see Related types.
- **v0.3** — `source` enum extended with the planning/ticketing tools:
  `jira`, `linear`, `slack` (messages/threads from designated channels),
  `asana`, and `trello`. Additive only; no field changes.
- **v0.2** — `source` enum extended with `intercom`, `otel`, `datadog`,
  `loopforge`, and `meta` (Merge0's own operational telemetry, ingested for
  the meta-loop). Additive only; no field changes.
- **v0.1** — initial schema.
- **Adapters may not invent fields.** Extensions go through schema versioning
  here, never through ad-hoc additions in an adapter.
- Vendor-specific data that has no schema home belongs in `raw`, which is for
  audit only and carries no compatibility promise.

## Signal

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | ULID string | yes | Assigned by the adapter at normalization time. Not stable across re-ingestion — use `fingerprint` for dedupe. |
| `source` | enum | yes | `posthog`, `sentry`, `zendesk`, `intercom`, `github`, `webhook`, `otel`, `datadog`, `loopforge`, `jira`, `linear`, `slack`, `asana`, `trello`, `meta` |
| `kind` | enum | yes | `exception`, `ux_friction`, `ticket`, `regression`, `custom` |
| `severity` | enum | yes | `low`, `medium`, `high`, `critical` |
| `source_ref` | string | yes | Vendor-native ID for the underlying object (issue ID, session ID, ticket ID). Deep links go in `evidence`. |
| `title` | string | yes | Short human-readable title. |
| `body` | string | yes | Normalized description. |
| `evidence` | `EvidenceLink[]` | yes (may be empty) | Replay URLs, stack traces, ticket threads, issue deep links. |
| `fingerprint` | string | yes | Stable hash for dedupe within a source: the same underlying defect must produce the same fingerprint across payload variants and re-ingestion. Format: `<source>:<16-byte-sha256-hex>`. |
| `join_keys` | `JoinKeys` | yes (fields optional) | Correlation context — **required where derivable** from the vendor payload. |
| `affected_count` | integer | no | Users/accounts impacted. |
| `delegated` | boolean | no (default `false`) | The signal was explicitly handed to Merge0 (e.g. a `merge0` label on the source ticket). Serialized only when `true`. |
| `first_seen` | RFC 3339 timestamp | yes | |
| `last_seen` | RFC 3339 timestamp | yes | |
| `raw` | JSON | yes | Original vendor payload, verbatim, for audit only. |

## JoinKeys

Join keys are what let triage correlate a Sentry exception with a PostHog
rage-click and a support ticket describing the same bug. They are part of the
schema from day one, not bolted on later. Every field is optional, but an
adapter that *can* derive one *must*.

| Field | Type | Description |
|---|---|---|
| `release` | string | Semver or commit SHA the signal was observed on. |
| `stack_hash` | string | Hash of the normalized stack location (culprit frame / error type), comparable across sources. |
| `account_id` | string | Vendor-side account/user identifier. |
| `url_path` | string | Path component only (no host, no query) of the URL where the signal occurred. |

Absent join keys are omitted from the serialized form entirely (not `null`).

## EvidenceLink

| Field | Type | Description |
|---|---|---|
| `kind` | enum | `replay`, `stack_trace`, `ticket`, `issue`, `other` |
| `label` | string | Human-readable label for the inbox/PR description. |
| `url` | string | Deep link back to the source tool. |

## Example

```json
{
  "id": "01J4YAND3RS0N5H0GQV8XKZ9TW",
  "source": "sentry",
  "source_ref": "5312345678",
  "kind": "exception",
  "severity": "high",
  "title": "TypeError: Cannot read properties of undefined (reading 'districtId')",
  "body": "Unhandled TypeError in SyncStatusPanel when a school has no linked district. 42 users affected since v2.3.0.",
  "evidence": [
    {
      "kind": "issue",
      "label": "Sentry issue CHALK-123",
      "url": "https://sentry.example.com/organizations/chalk/issues/5312345678/"
    },
    {
      "kind": "stack_trace",
      "label": "Latest event stack trace",
      "url": "https://sentry.example.com/organizations/chalk/issues/5312345678/events/latest/"
    }
  ],
  "fingerprint": "sentry:9f86d081884c7d659a2feaa0c55ad015",
  "join_keys": {
    "release": "v2.3.0",
    "stack_hash": "a3b1c2d4e5f60718293a4b5c6d7e8f90",
    "url_path": "/districts/sync"
  },
  "affected_count": 42,
  "first_seen": "2026-08-01T04:12:00Z",
  "last_seen": "2026-08-06T22:30:00Z",
  "raw": {
    "id": "5312345678",
    "level": "error"
  }
}
```

## Adapter conformance

An adapter is conformant when its golden-payload tests pass: recorded vendor
payloads in, expected Signals out, byte-identical after normalizing volatile
fields (`id`). The shared harness lives in `crates/merge0-adapters` and is the
same one community adapters are expected to use.

## Related types

`WorkOrder`, `OutcomeRef`, and `GateConfidence` (defined in
`crates/merge0-signal` alongside `Signal`) are internal pipeline types, not
part of the adapter integration surface; their canonical definition is the
Rust source and the PRD (§4). `GateConfidence` (`low` / `medium` / `high`,
default `low`) is the gate's self-assessed fix confidence carried on each
Work Order; unparseable model output maps to `low` so autonomy decisions
fail conservative.
