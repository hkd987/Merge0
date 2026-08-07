# ee/ — commercial directory (placeholder)

Everything outside this directory is MIT. This directory holds the commercial
side of the open-core split (PRD, "Distribution & Open Source Strategy"):

- Multi-tenant org management, SSO/SAML, RBAC, audit log
- Cross-tenant outcome priors (hosted-only data service)
- Curated signed skill registry (P2)
- Credential broker as a managed service
- Billing, usage metering, hosted convenience

Empty until Phase 2. The boundary exists from the first commit so the MIT core
is honest by construction: nothing in `crates/` may depend on anything in
`ee/`.
