# Security Policy

Merge0 receives webhooks from the public internet, holds a GitHub App
private key and vendor API credentials, brokers short-lived tokens to
runners, and dispatches an autonomous agent with write access to your
repository. We treat reports about any of those surfaces as high priority.

## Reporting a vulnerability

**Please do not open a public issue for security reports.**

Report privately via **GitHub's private vulnerability reporting**: on the
repository page, *Security → Report a vulnerability*. This reaches the
maintainers without exposing the report, lets us collaborate on a fix in a
private fork, and credits you in the resulting advisory if you want credit.

Please include what you can of: the affected endpoint/crate, reproduction
steps, and impact as you understand it. Proof-of-concept payloads are
welcome in the private report — never in a public issue, and we ask the
same discipline of reporters that Merge0's own gate applies to its Work
Orders: describe the weapon, don't publish it.

## What to expect

- **Acknowledgement within 72 hours**, an assessment within a week.
- Confirmed vulnerabilities get a fix on the fastest branch we can manage,
  a GitHub Security Advisory, and — per this repo's own engineering rule
  (CLAUDE.md invariant 9) — a regression test or lint that is verified to
  fail on the reintroduced bug before the fix is called done.
- We'll coordinate disclosure timing with you; our default is to publish
  the advisory when the fix ships.

## Scope notes for researchers

- The threat model and the most recent internal review live in
  [`docs/security-review.md`](docs/security-review.md) — including what has
  and has not been hardened, stated plainly. Reports that extend that
  document's "known gaps" section are explicitly in scope.
- Self-hosted deployments are configured by their operators; a
  misconfigured deployment (e.g. `MERGE0_API_TOKEN` unset, which the server
  loudly warns about) is not by itself a vulnerability, but a way to make
  a *correctly* configured deployment unsafe is.
- The `merge0` delegation label and signal content are untrusted input by
  design — prompt-injection paths from signal text into gate decisions or
  Work Orders are in scope and among the reports we care most about.

## Supported versions

Pre-1.0, only the latest release (and `main`) receive security fixes.
