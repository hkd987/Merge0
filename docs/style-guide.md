# Merge0 UI style guide

This document is the contract for every pixel Merge0 ships. All UI work —
new pages, tweaks, future surfaces — follows it. Colors, type, spacing,
and radii come exclusively from the tokens in `ui/src/theme.css`; a test
(`ui/src/__tests__/style-lint.test.ts`) fails the build if a component
hardcodes a color literal. Change the system by changing the tokens and
this document in the same PR — never by special-casing a component.

## Identity

High-contrast mono, one electric accent. Merge0's UI is an instrument, not
a brochure: near-black on near-white (and the inverse in dark mode), thin
1px borders instead of shadow stacks, sharp corners, dense-but-calm
spacing, and a single electric blue reserved for interaction and identity.
Severity communicates through its own semantic colors, never through the
accent.

## Tokens (`ui/src/theme.css` is the source of truth)

### Color

| Token | Light | Dark | Use |
|---|---|---|---|
| `--bg` | `#fdfdfc` | `#0c0d0f` | page ground |
| `--surface` | `#ffffff` | `#131519` | cards, nav, dialogs |
| `--surface-2` | `#f4f4f2` | `#1b1e24` | code blocks, wells, skeletons |
| `--ink` | `#111214` | `#f2f2f0` | primary text |
| `--ink-2` | `#5c6066` | `#9ba0a8` | secondary text |
| `--line` | `#e3e3e0` | `#272b33` | borders, dividers |
| `--accent` | `#2f6fff` | `#5b8dff` | interaction: links, primary buttons, focus, active nav |
| `--accent-ink` | `#ffffff` | `#0c0d0f` | text on accent |
| `--sev-critical` | `#d92d20` | `#f97066` | severity only |
| `--sev-high` | `#dc6803` | `#fdb022` | severity only |
| `--sev-medium` | `#a15c07` | `#d8b437` | severity only |
| `--sev-low` | `#087443` | `#4cc38a` | severity only |
| `--ok` | `#087443` | `#4cc38a` | success/verified states |
| `--danger` | `#d92d20` | `#f97066` | destructive actions, errors |

Rules:
- The accent is for **interaction and identity** (buttons, links, focus
  ring, active nav, the gate-progress bar). It never encodes severity or
  status — those use the semantic tokens.
- Severity chips/stripes pair the severity color with `--surface` tints,
  never solid fills behind long text.
- Both themes ship first-class: define light on `:root`, dark under
  `@media (prefers-color-scheme: dark)` AND `[data-theme="dark"]`; every
  component styles through tokens only, so it cannot render one-themed.

### Type

- **IBM Plex Sans** — body, headings, buttons (self-hosted via
  `@fontsource/ibm-plex-sans`; never a CDN — the UI must work air-gapped).
- **IBM Plex Mono** — data: metrics, ids, timestamps, code, uppercase
  labels/eyebrows, kbd hints (`@fontsource/ibm-plex-mono`).
- Scale (rem): 0.75 (labels/mono-small), 0.8125 (secondary), 0.9375
  (body), 1.125 (card title), 1.5 (page title), 2.25 (hero metric).
- All numerals in metrics use `font-variant-numeric: tabular-nums`.
- Uppercase labels get `letter-spacing: 0.08em`.

### Space, shape, motion

- Spacing grid: 8px base — use `--s1..--s6` (4, 8, 12, 16, 24, 32).
- Radii: `--r` = 6px (cards, dialogs, buttons), `--r-s` = 4px (chips,
  badges, kbd). Nothing rounder.
- Depth: 1px `--line` borders. One shadow token (`--shadow`) for floating
  layers only (dialogs, toasts).
- Motion: 120ms ease-out on hover/focus/enter; all transitions wrapped in
  `@media (prefers-reduced-motion: no-preference)`.
- Focus: every interactive element shows `outline: 2px solid
  var(--accent); outline-offset: 2px` on `:focus-visible`. Never remove
  an outline without replacing it.

## Components

- **Nav bar**: product wordmark (Plex Mono, "merge0"), links Inbox /
  Dashboard / Setup; active link = accent underline; right side holds the
  token status ("connected" dot / "set token" button).
- **Report card**: 3px severity stripe on the left edge; header row =
  severity badge + title (one line, ellipsized) + mono timestamp; body =
  summary (max 3 lines), evidence chips, footer actions. Focused card
  (keyboard) gets a 2px accent border. Opportunity cards use a
  distinguishable stripe (`--ink-2`) + "Opportunity" badge, no actions.
- **Buttons**: primary (accent fill) for the single main action per view;
  ghost (border, ink) for secondary; danger for destructive. Verbs only:
  "Approve → dispatch", "Dismiss…", "Copy".
- **Evidence chip**: mono label in a bordered pill, opens in a new tab,
  external-link affordance.
- **Stat tile**: mono uppercase label, hero number (tabular), optional
  sub-line. The Phase-0 gate tile shows a progress bar toward 60% (accent
  fill; `--ok` when met).
- **Dialogs** replace `prompt()`/`alert()`: token-gate modal (asks once,
  stores in localStorage, re-raises on 401), dismiss-reason dialog
  (radio list of the four structured reasons, keyboard 1–4).
- **Toast**: bottom-right, mono, auto-dismiss 4s; errors persist until
  dismissed and say what to do next.
- **States**: every async view implements loading (skeleton blocks, no
  spinners), empty (inbox zero: "Inbox zero — a quiet inbox that's always
  right."), error (message + Retry button), and 401 (token modal).
- **Kbd hints**: `<kbd>` in Plex Mono with `--r-s` border, shown in the
  inbox footer: `j`/`k` move · `a` approve · `d` dismiss · `1–4` reason.

## Interaction principles (from the PRD, non-negotiable)

1. **The inbox is a review queue, not a dashboard** (§6): newest-first,
   one-screen decision per report, evidence above the fold, zero
   configuration surfaces in the review path. Anything that grows median
   time-to-review is a regression regardless of how useful it looks.
2. **Keyboard-first**: the whole review loop must be completable without
   a pointer; hints stay visible.
3. **Auth model**: pages are data-free static assets on the open router;
   the token lives in the browser and rides every JSON call (audit C2).
   No server-rendered data, ever.
4. **Copy**: buttons say what happens; errors say what went wrong and
   what to do; metrics are labeled in product language (merge rate, gate
   precision, runner yield), not schema language.
5. **Accessibility**: WCAG AA contrast on both themes; visible focus;
   reduced-motion respected; interactive targets ≥ 32px tall.

## Dev loop

- `cd ui && npm run dev` — Vite dev server on :5173, `/api` proxied to
  `127.0.0.1:8080` (run the server with `MERGE0_DEV_FAKES=1`).
- `npm run build` → `ui/dist`, embedded into `merge0-server` at compile
  time (`rust-embed`); rebuild the server after a UI build.
- `npm test` — Vitest: unit + component smokes + the style-lint test.
