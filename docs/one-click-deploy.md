# One-click deploys

Three hosted paths for people who don't want to run `docker compose`.
Every path ends in the same place: a `merge0-server` with a Postgres,
ready for the `/setup` onboarding flow. Whatever the platform, the same
rules hold — secrets go into the platform's env store by NAME, tokens are
three long random strings that must differ, and after the first deploy
you set `MERGE0_PUBLIC_URL` to the URL the platform gave you so Slack
links and runner callbacks resolve.

To try Merge0 without any credentials, set `MERGE0_DEV_FAKES=1` — the
whole loop runs against an in-process fake model and fake GitHub. Loudly
not for production.

## Render

[![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/hkd987/Merge0)

`render.yaml` is the blueprint: a Docker web service plus a managed
Postgres, tokens generated server-side, your credentials prompted at
deploy time. Free plans by default — upgrade the database before trusting
it with anything you'd miss (free Postgres instances expire).

## Railway

Railway deploys arrive through a published **template** (that is also
what the deploy button links to). The template is maintained in the
Railway dashboard by the repo owner; `railway.json` in this repo carries
the build/deploy config every Railway deploy uses (Dockerfile build,
`/healthz` health check, restart policy).

Publishing/updating the template (owner-only, one time):

1. railway.com → New Template → add a service from this GitHub repo, and
   a Postgres database service.
2. On the app service set the variables from `.env.example`, with
   `MERGE0_DATABASE_URL = ${{Postgres.DATABASE_URL}}` and
   `MERGE0_BIND = 0.0.0.0:${{PORT}}` (Railway injects `PORT`), marking
   the credential ones as required user inputs.
3. Publish, then put the resulting `railway.com/template/...` URL behind
   the Railway button in README.md and site/index.html.

Railway's template marketplace pays template publishers a share of the
usage their template generates (their "kickback" program) — the template
must be published from the account that should receive it.

## Fly.io

```sh
fly launch --from https://github.com/hkd987/Merge0   # reads fly.toml
fly postgres create && fly postgres attach <pg-app>
fly secrets set MERGE0_REPO=owner/name MERGE0_API_TOKEN=... # …see fly.toml header
```

`fly.toml` keeps one machine always running — the scheduler polls your
vendors on an interval, and a scaled-to-zero machine polls nothing.

## What no platform can do for you

- The **GitHub App** (five minutes, `docs/github-app-setup.md`) — Merge0
  authenticates as an App, never a PAT.
- The three files in your product repo from `/setup`, and the Actions
  secrets for your chosen agent (README “Bring your own agent”).
- Vendor credentials for the sources you enable in `config/sources.toml`.
