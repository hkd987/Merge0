# Hosted deployment runbook (ee)

How to run Merge0 as a multi-tenant hosted service. Everything here is
`ee/`-licensed machinery (see `ee/LICENSE`); the per-tenant data plane it
launches is the ordinary MIT `merge0-server`.

## Architecture in one paragraph

One Postgres cluster. One **control plane** (`merge0-hosted`) owning the
`merge0_control` schema: tenant lifecycle, membership + RBAC, audit,
metering, cross-tenant priors. One **data plane per tenant** — a stock
`merge0-server` process pointed at the tenant schema the control plane
provisioned (`MERGE0_TENANT`), carrying that tenant's own GitHub App
credentials and API keys. Tenant isolation is schema-per-tenant at the
database and process-per-tenant at runtime; nothing multi-tenant executes
inside the data plane, which is why the MIT core needs no tenant logic.

**Never point two data-plane processes at the same tenant schema.** The
scheduler and cursors assume a single writer.

## Provisioning a tenant

1. `POST /ee/tenants` (operator token + `x-merge0-actor`) with
   `{name, plan, admin_email}` — provisions the schema, seeds the admin
   seat, audits `tenant.created`.
2. `GET /ee/tenants/{id}/runtime` — the deploy manifest: desired state, the
   `MERGE0_TENANT` value, and the exact env-var **names** the operator must
   supply (values never transit the control plane — same name-only
   discipline as `config/sources.toml`).
3. Add a `tenant-*` service from the template in
   `docker-compose.hosted.yml` (or your orchestrator's equivalent), fill in
   that tenant's credentials, deploy.
4. The tenant commits the three files from their instance's `/setup` page
   to their repo and configures the two Actions secrets — the same
   onboarding as self-hosted, per tenant.

## Suspension

`POST /ee/tenants/{id}/suspend` freezes the org three ways:

- the runtime manifest's `desired_state` flips to `"suspended"` — your
  orchestrator (or you) scales the tenant service to 0;
- the control plane refuses to open the tenant's schema (`tenant_store`),
  so control-surface writes stop;
- membership edits are refused (409) until resume — a frozen org's roster
  must not drift.

Two things deliberately keep working: **usage/metering** (the invoice that
justified the suspension must remain computable) and the **audit trail**.
`POST /ee/tenants/{id}/resume` reverses everything; both actions are
audited with the acting operator.

## Metering and invoicing

`GET /ee/tenants/{id}/usage?window_days=30&pricing=per_merged_pr:1000`
returns the window's merged/dispatched/opened counts, tokens on merged
runs, and a priced invoice under either model
(`per_merged_pr:<cents>` or `flat:<monthly>:<included>:<overage>`).
Payment collection is out of scope — this is the metering source of truth
a billing system consumes, not the billing system.

## Cross-tenant priors

`GET /ee/priors` aggregates merge-rate priors across all non-suspended
tenants into anonymized severity/source-mix buckets (no titles, no repo
names, no tenant identifiers — enforced by test). Its `gate_context` block
can be wired into any tenant's gate via the MIT-generic
`MERGE0_GATE_CONTEXT_EXTRA` env var; the gate prompt labels it as
background evidence, never instructions. Refresh it on deploys — it is a
snapshot, not a live feed.

## Offboarding

Remove the tenant's data-plane service, then drop the schema via operator
tooling (`TenantManager::store().drop_tenant(schema)` — destructive,
unaudited beyond your own change control, so gate it accordingly). Export
anything contractually owed first; there is no undo.

## Operational notes

- **Backups**: one Postgres cluster carries every tenant — your backup
  cadence is a promise you're making to all of them at once.
- **The control plane is not internet-facing by need.** It serves
  operators, not tenants; keep it on an internal network or behind your
  admin VPN. The per-tenant data planes are the tenant-facing surface.
- **`MERGE0_EE_ADMIN_TOKEN` is root.** The RBAC matrix governs *actors on
  tenants*; the token itself is the operator console's credential.
  Rotate it like one.
- **What this is not yet**: SSO/SAML (roadmap, PRD), self-serve signup
  (tenants are provisioned by you), automated reconciliation (the runtime
  manifest is built for an orchestrator loop, but shipping one is not this
  repo's job), payment collection.
