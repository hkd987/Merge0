# Creating the Merge0 GitHub App

Merge0 talks to GitHub as a **GitHub App**, never with a personal access
token. This is the one setup step that happens outside Merge0, and it
takes about five minutes. At the end you will have four values for your
`.env` file.

> **Self-hosting only.** On the hosted service you click *Install* and
> skip this page entirely — the App already exists.

Why an App rather than a PAT: installation tokens are scoped to the repos
you pick, expire after an hour, are minted per request, and are revoked by
uninstalling — none of which is true of a PAT tied to one human's account.

## What you will end up with

| Value | Goes into | Where you get it |
|---|---|---|
| App ID | `MERGE0_GITHUB_APP_ID` | App settings page, after step 1 |
| Private key (PEM) | `MERGE0_GITHUB_APP_PRIVATE_KEY` | Downloaded file, step 3 |
| Installation ID | `MERGE0_GITHUB_INSTALLATION_ID` | Install URL, step 4 |
| Webhook secret | `MERGE0_GITHUB_WEBHOOK_SECRET` | You invent it, step 1 |

Have your server's public HTTPS URL ready before you start — GitHub must
be able to reach it to deliver webhooks. If you are only evaluating
locally, see [Local evaluation](#local-evaluation) below.

## 1. Create the App

Go to **Settings → Developer settings → GitHub Apps → New GitHub App**.

- For a company repo, create it under the **organization**
  (`https://github.com/organizations/YOUR-ORG/settings/apps/new`), not
  your personal account — a personal App dies with your account access.
- Anyone creating it needs org owner permission.

Fill in:

| Field | Value |
|---|---|
| **GitHub App name** | Anything unique, e.g. `merge0-yourcompany` |
| **Homepage URL** | Your Merge0 URL (or your company site) |
| **Webhook** | ✅ Active |
| **Webhook URL** | `https://your-merge0-host/webhooks/github` |
| **Webhook secret** | A long random string — generate with `openssl rand -hex 32` and save it |
| **Where can this App be installed?** | Only on this account |

The webhook secret is what lets Merge0 prove a delivery really came from
GitHub. Merge0 rejects every unsigned webhook, so this is not optional.

## 2. Set permissions and events

Under **Repository permissions**, set exactly these five. Everything else
stays *No access* — this is the complete list Merge0's code actually uses,
and each one buys a specific capability:

| Permission | Access | Why Merge0 needs it |
|---|---|---|
| **Administration** | Read-only | Read branch protection, to refuse dispatch on unprotected repos (P0-9) |
| **Contents** | Read and write | Trigger the runner workflow, create fix branches, read `MERGE0.md` / `agent.toml`, read releases |
| **Issues** | Read-only | Ingest GitHub Issues as signals (skip if you don't use that source) |
| **Metadata** | Read-only | Mandatory; GitHub selects it automatically |
| **Pull requests** | Read and write | Open the fix PRs |

Merge0 never asks for Actions, Packages, Secrets, or member/org
permissions. It cannot merge its own PRs — merging stays a human action
protected by your branch protection.

Then under **Subscribe to events**, tick exactly three:

- ☑️ **Pull request** — records merged/closed outcomes (the merge-rate metric)
- ☑️ **Push** — detects reverts of merged Merge0 PRs (hard negatives)
- ☑️ **Release** — builds the release timeline for first-bad-release attribution

Click **Create GitHub App**.

## 3. Note the App ID and generate a private key

You are now on the App's settings page.

1. Copy the **App ID** near the top → `MERGE0_GITHUB_APP_ID`.
2. Scroll to **Private keys** → **Generate a private key**. A `.pem` file
   downloads. **This is the only copy** — GitHub cannot show it again.

Treat that file like a password: it can mint tokens for every repo the App
is installed on. If it leaks, delete the key on this page and generate a
new one; old keys stop working immediately.

## 4. Install the App and get the Installation ID

On the left, click **Install App** → **Install** next to your account or
org.

Choose **Only select repositories** and pick the repo Merge0 will open PRs
against. Selecting *All repositories* grants more than Merge0 needs today.

After installing you land on a URL like:

```
https://github.com/settings/installations/12345678
                                          ^^^^^^^^
```

That trailing number is your **Installation ID** →
`MERGE0_GITHUB_INSTALLATION_ID`. For an org install the URL is
`https://github.com/organizations/YOUR-ORG/settings/installations/12345678`
— same trailing number.

If you navigated away: the App's settings page → **Install App** → the
gear icon next to your installed account returns you to that URL.

## 5. Put the values in `.env`

```sh
MERGE0_GITHUB_APP_ID=123456
MERGE0_GITHUB_INSTALLATION_ID=12345678
MERGE0_GITHUB_WEBHOOK_SECRET=the-random-string-from-step-1
MERGE0_GITHUB_APP_PRIVATE_KEY="-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----"
```

### The private key needs quotes

This is the step that trips people up. The `.pem` file is multi-line, but
an unquoted multi-line value is a **hard parse error** in a `.env` file —
docker compose refuses to read the whole file (`key cannot contain a
space`). Two forms work:

**One line with `\n` escapes (recommended — works everywhere, including
PaaS environment-variable fields that only accept one line):**

```sh
# Prints the correctly escaped, quoted line — paste the output into .env:
awk 'BEGIN{printf "MERGE0_GITHUB_APP_PRIVATE_KEY=\""} {printf "%s\\n", $0} END{print "\""}' \
  your-app.private-key.pem
```

**Or a quoted multi-line block** (docker compose ≥ 2.x; some other tools
disagree, which is why the single-line form is recommended):

```sh
MERGE0_GITHUB_APP_PRIVATE_KEY="-----BEGIN RSA PRIVATE KEY-----
MIIEow...
-----END RSA PRIVATE KEY-----"
```

Merge0 accepts the key with real newlines, with `\n` escapes, and with
stray surrounding quotes — but it cannot help if the `.env` file itself
fails to parse, which is what the unquoted form causes.

Also set `MERGE0_PUBLIC_URL` to the same public HTTPS origin you used for
the webhook URL. GitHub and your vendors call back to it.

## 6. Verify

Start Merge0 (`docker compose up --build`) and check, in order:

```sh
curl -s https://your-merge0-host/healthz
# {"ok":true}  — server + database are up

curl -s -H "authorization: Bearer $MERGE0_API_TOKEN" \
  https://your-merge0-host/safety
# {"satisfied":true,...}  — App credentials work AND the repo is protected
```

`/safety` is the real end-to-end proof: returning it at all means Merge0
authenticated as the App, minted an installation token, and read your
repo's branch protection.

If it reports `"satisfied": false` with `default branch has no branch
protection` or `no required status checks configured`, the App is working
correctly — your repo just isn't protected yet. Merge0 refuses to dispatch
until it is, deliberately: the agent's PRs must land against the same
required checks a human's do. Fix it in **Settings → Branches** on the
repo.

Then open `/setup` in a browser for the three files to commit and the two
Actions secrets to add.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| compose: `failed to read .env: key cannot contain a space` | Unquoted multi-line PEM | Quote it — see [above](#the-private-key-needs-quotes) |
| `bad private key: InvalidKeyFormat` | Key isn't a valid PEM (truncated paste, or a PKCS#8 conversion gone wrong) | Re-copy the downloaded `.pem` verbatim; regenerate in step 3 if lost |
| `401 Bad credentials` on startup | App ID doesn't match the private key | Confirm the App ID is from the same App page you generated the key on |
| `404` reading the repo | App not installed on that repo, or `MERGE0_REPO` typo | Step 4 — check the repo is in the install's selected list |
| `403` on dispatch or PR creation | Missing permission | Re-check the step-2 table; after changing permissions you must **accept the new request** under the installation |
| Webhooks never arrive | Wrong URL, or the host isn't publicly reachable | App settings → **Advanced** → *Recent Deliveries* shows every attempt and its response |
| Webhooks arrive but are rejected | Secret mismatch | `MERGE0_GITHUB_WEBHOOK_SECRET` must equal the App's webhook secret exactly |

**Changed permissions after installing?** GitHub does not apply them
silently — the org owner gets a request to approve under **Settings →
Applications → Installed GitHub Apps → Configure**. Until it is approved
the App keeps its old, narrower access.

## Local evaluation

To try Merge0 before exposing a host, run it with dev fakes — no GitHub
App required at all:

```sh
MERGE0_DEV_FAKES=1 docker compose up --build
```

The model and GitHub are replaced by in-process fakes; every other code
path (ingest, triage, gate, budgets, telemetry) is real. It logs a loud
warning so this can never be mistaken for production. `scripts/e2e-manual.sh`
drives the entire loop this way.

To test *real* webhooks locally, expose your port with a tunnel
(`cloudflared tunnel --url http://localhost:8080`) and use that HTTPS URL
as both the App's webhook URL and `MERGE0_PUBLIC_URL`.
