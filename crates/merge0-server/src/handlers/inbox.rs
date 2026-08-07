//! `GET /inbox` — the web inbox (PRD §6, §6a item 4): a review queue, not a
//! dashboard. This route serves a STATIC SHELL containing no report data
//! (which is why it can live on the open router): the page asks for the API
//! token once, keeps it in localStorage, and loads everything through the
//! authenticated JSON endpoints. That resolves the audit's C2 inversion —
//! the inbox now works precisely when auth is ON.
//!
//! Keyboard: j/k move, a approve, d dismiss (then 1–4 for the reason).

use axum::response::Html;

pub async fn page() -> Html<&'static str> {
    Html(SHELL)
}

const SHELL: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Merge0 inbox</title>
<style>
  body { font: 15px/1.5 system-ui, sans-serif; margin: 2rem auto; max-width: 60rem; padding: 0 1rem; }
  .report { border: 1px solid #ccc; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }
  .report.focused { outline: 3px solid #4a90d9; }
  .sev { font-weight: 700; text-transform: uppercase; font-size: 12px; padding: 2px 8px; border-radius: 999px; }
  .sev-critical { background: #ffd6d6; } .sev-high { background: #ffe4c4; }
  .sev-medium { background: #fff3bf; } .sev-low { background: #e6f4ea; }
  .evidence a { margin-right: 0.75rem; }
  .actions button { margin-right: 0.5rem; padding: 0.4rem 0.9rem; cursor: pointer; }
  .kbd-help, .status { color: #666; font-size: 13px; }
  .opportunity { border-left: 6px solid #7b61c4; }
</style></head>
<body>
<h1>Merge0 inbox</h1>
<p class="kbd-help">j/k: move &nbsp; a: approve &nbsp; d: dismiss (then 1–4 for reason)</p>
<p class="status" id="status">loading…</p>
<div id="reports"></div>
<script>
const REASONS = ['intended_behavior','wont_fix','duplicate','bad_evidence'];
let focus = 0;

function token() {
  let t = localStorage.getItem('merge0_token');
  if (t === null) {
    t = prompt('Merge0 API token (leave empty for an unauthenticated dev server):') || '';
    localStorage.setItem('merge0_token', t);
  }
  return t;
}
function headers(extra) {
  const h = Object.assign({'content-type': 'application/json'}, extra || {});
  const t = token();
  if (t) h['authorization'] = `Bearer ${t}`;
  return h;
}
async function api(path, options) {
  const res = await fetch(path, Object.assign({headers: headers()}, options || {}));
  if (res.status === 401) {
    localStorage.removeItem('merge0_token');
    document.getElementById('status').textContent = 'unauthorized — reload to re-enter the token';
    throw new Error('unauthorized');
  }
  return res;
}
function esc(text) {
  const div = document.createElement('div');
  div.textContent = text == null ? '' : String(text);
  return div.innerHTML;
}
function render(reports) {
  const root = document.getElementById('reports');
  root.innerHTML = '';
  document.getElementById('status').textContent = reports.length
    ? `${reports.length} report(s)` : "Inbox zero — a quiet inbox that's always right.";
  for (const r of reports) {
    const card = document.createElement('div');
    const handedOff = r.status === 'handed_off';
    card.className = 'report' + (handedOff ? ' opportunity' : '');
    card.dataset.id = r.id;
    card.dataset.actionable = String(r.status === 'awaiting_review');
    const evidence = (r.evidence || []).map(e =>
      `<a href="${esc(e.url)}" target="_blank" rel="noopener">${esc(e.label)}</a>`).join(' ');
    card.innerHTML = `
      <span class="sev sev-${esc(r.severity)}">${esc(r.severity)}</span>
      <strong>${esc(r.title)}</strong>
      <div>affected: ${esc(r.affected_count ?? '?')} · release: ${esc(r.suspect_release ?? '—')} · ${esc(r.created_at)}</div>
      <p>${esc(r.summary).replace(/\n/g, '<br>')}</p>
      <div class="evidence">${evidence}</div>
      ${handedOff ? '<em>Opportunity — handed off, no PR will be generated</em>' : ''}
      ${r.status === 'awaiting_review' ? `<div class="actions">
        <button onclick="act('${r.id}','approve')">Approve → dispatch</button>
        <button onclick="dismissPrompt('${r.id}')">Dismiss…</button>
      </div>` : ''}`;
    root.appendChild(card);
  }
  refocus();
}
async function load() {
  const [awaiting, handed] = await Promise.all([
    api('/reports?status=awaiting_review').then(r => r.json()),
    api('/reports?status=handed_off').then(r => r.json()),
  ]);
  const all = awaiting.concat(handed);
  all.sort((a, b) => b.created_at.localeCompare(a.created_at));
  render(all);
}
async function act(id, path, body) {
  const res = await api(`/reports/${id}/${path}`, {method: 'POST',
    body: body ? JSON.stringify(body) : null});
  const out = await res.json();
  if (!res.ok) alert(`error: ${out.error}`);
  load();
}
function dismissPrompt(id) {
  const n = prompt('Dismiss reason: 1 intended behavior, 2 wont fix, 3 duplicate, 4 bad evidence');
  const reason = REASONS[Number(n) - 1];
  if (reason) act(id, 'dismiss', {reason});
}
function cards() { return [...document.querySelectorAll('.report')]; }
function refocus() {
  cards().forEach((c, i) => c.classList.toggle('focused', i === focus));
  const c = cards()[focus];
  if (c) c.scrollIntoView({block: 'nearest'});
}
document.addEventListener('keydown', e => {
  const cs = cards();
  if (e.key === 'j') { focus = Math.min(focus + 1, cs.length - 1); refocus(); }
  if (e.key === 'k') { focus = Math.max(focus - 1, 0); refocus(); }
  const c = cs[focus];
  if (!c) return;
  if (e.key === 'a' && c.dataset.actionable === 'true') act(c.dataset.id, 'approve');
  if (e.key === 'd' && c.dataset.actionable === 'true') dismissPrompt(c.dataset.id);
});
load().catch(() => {});
</script>
</body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_contains_no_data_and_authenticates_client_side() {
        // The shell is served unauthenticated, so it must be pure chrome:
        // no report fields, and every data call goes through the token'd
        // fetch wrapper.
        assert!(SHELL.contains("localStorage.getItem('merge0_token')"));
        assert!(SHELL.contains("authorization"));
        assert!(SHELL.contains("/reports?status=awaiting_review"));
        // Client-side escaping goes through textContent.
        assert!(SHELL.contains("div.textContent"));
    }
}
