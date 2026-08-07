//! `GET /inbox` — the web inbox (PRD §6, §6a item 4): a review queue, not a
//! dashboard. Newest first, one-screen decision per report (evidence above
//! the fold), keyboard-actionable approve/dismiss (j/k to move, a to
//! approve, d to dismiss), no configuration surfaces in the review path.

use super::ApiError;
use crate::AppState;
use axum::extract::State;
use axum::response::Html;
use merge0_signal::{Report, ReportStatus};

pub async fn page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let mut reports = state
        .tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await?;
    let handed_off = state
        .tenant
        .list_reports(Some(ReportStatus::HandedOff))
        .await?;
    reports.extend(handed_off);
    reports.sort_by_key(|item| std::cmp::Reverse(item.created_at));
    Ok(Html(render(&reports)))
}

fn render(reports: &[Report]) -> String {
    let rows: String = reports.iter().map(render_report).collect();
    let empty = if reports.is_empty() {
        "<p class=\"empty\">Inbox zero — a quiet inbox that's always right.</p>"
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Merge0 inbox</title>
<style>
  body {{ font: 15px/1.5 system-ui, sans-serif; margin: 2rem auto; max-width: 60rem; padding: 0 1rem; }}
  .report {{ border: 1px solid #ccc; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }}
  .report.focused {{ outline: 3px solid #4a90d9; }}
  .sev {{ font-weight: 700; text-transform: uppercase; font-size: 12px; padding: 2px 8px; border-radius: 999px; }}
  .sev-critical {{ background: #ffd6d6; }} .sev-high {{ background: #ffe4c4; }}
  .sev-medium {{ background: #fff3bf; }} .sev-low {{ background: #e6f4ea; }}
  .evidence a {{ margin-right: 0.75rem; }}
  .actions button {{ margin-right: 0.5rem; padding: 0.4rem 0.9rem; cursor: pointer; }}
  .kbd-help {{ color: #666; font-size: 13px; }}
  .opportunity {{ border-left: 6px solid #7b61c4; }}
</style></head>
<body>
<h1>Merge0 inbox</h1>
<p class="kbd-help">j/k: move &nbsp; a: approve &nbsp; d: dismiss (then 1–4 for reason)</p>
{empty}{rows}
<script>
const cards = [...document.querySelectorAll('.report')];
let focus = 0;
const REASONS = ['intended_behavior','wont_fix','duplicate','bad_evidence'];
function refocus() {{ cards.forEach((c,i) => c.classList.toggle('focused', i === focus));
  if (cards[focus]) cards[focus].scrollIntoView({{block:'nearest'}}); }}
async function act(id, path, body) {{
  const res = await fetch(`/reports/${{id}}/${{path}}`, {{method:'POST',
    headers:{{'content-type':'application/json'}}, body: body ? JSON.stringify(body) : null}});
  const out = await res.json();
  alert(res.ok ? `${{path}} ok` : `error: ${{out.error}}`);
  if (res.ok) location.reload();
}}
document.addEventListener('keydown', e => {{
  if (e.key === 'j') {{ focus = Math.min(focus + 1, cards.length - 1); refocus(); }}
  if (e.key === 'k') {{ focus = Math.max(focus - 1, 0); refocus(); }}
  if (!cards[focus]) return;
  const id = cards[focus].dataset.id;
  const actionable = cards[focus].dataset.actionable === 'true';
  if (e.key === 'a' && actionable) act(id, 'approve');
  if (e.key === 'd' && actionable) {{
    const n = prompt('Dismiss reason: 1 intended behavior, 2 wont fix, 3 duplicate, 4 bad evidence');
    const reason = REASONS[Number(n) - 1];
    if (reason) act(id, 'dismiss', {{reason}});
  }}
}});
refocus();
</script>
</body></html>"#
    )
}

fn render_report(report: &Report) -> String {
    let evidence: String = report
        .evidence
        .iter()
        .map(|e| {
            format!(
                "<a href=\"{url}\" target=\"_blank\" rel=\"noopener\">{label}</a>",
                url = escape(&e.url),
                label = escape(&e.label),
            )
        })
        .collect();
    let severity = format!("{:?}", report.severity).to_lowercase();
    let actionable = report.status == ReportStatus::AwaitingReview;
    let opportunity_class = if report.status == ReportStatus::HandedOff {
        " opportunity"
    } else {
        ""
    };
    let badge = if report.status == ReportStatus::HandedOff {
        "<em>Opportunity — handed off, no PR will be generated</em>"
    } else {
        ""
    };
    let buttons = if actionable {
        format!(
            r#"<div class="actions">
  <button onclick="act('{id}','approve')">Approve → dispatch</button>
  <button onclick="const n = prompt('1 intended, 2 wontfix, 3 dup, 4 bad evidence');
    const r = ['intended_behavior','wont_fix','duplicate','bad_evidence'][Number(n)-1];
    if (r) act('{id}','dismiss',{{reason:r}})">Dismiss…</button>
</div>"#,
            id = report.id
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="report{opportunity_class}" data-id="{id}" data-actionable="{actionable}">
  <span class="sev sev-{severity}">{severity}</span>
  <strong>{title}</strong>
  <div>affected: {affected} · release: {release} · {created}</div>
  <p>{summary}</p>
  <div class="evidence">{evidence}</div>
  {badge}
  {buttons}
</div>"#,
        id = report.id,
        title = escape(&report.title),
        affected = report
            .affected_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into()),
        release = report
            .suspect_release
            .as_deref()
            .map(escape)
            .unwrap_or_else(|| "—".into()),
        created = report.created_at.format("%Y-%m-%d %H:%M UTC"),
        summary = escape(&report.summary).replace('\n', "<br>"),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use merge0_signal::{EvidenceKind, EvidenceLink, ReportKind, Severity};
    use ulid::Ulid;

    #[test]
    fn render_escapes_untrusted_signal_content() {
        let report = Report {
            id: Ulid::new(),
            kind: ReportKind::Maintenance,
            title: "<script>alert(1)</script>".into(),
            summary: "user & <payload>".into(),
            severity: Severity::High,
            evidence: vec![EvidenceLink {
                kind: EvidenceKind::Issue,
                label: "\"quoted\"".into(),
                url: "https://example.com/x?a=1&b=2".into(),
            }],
            signal_ids: vec![Ulid::new()],
            fingerprints: vec!["f".into()],
            suspect_release: None,
            affected_count: Some(3),
            status: ReportStatus::AwaitingReview,
            created_at: Utc::now(),
        };
        let html = render(&[report]);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("user &amp; &lt;payload&gt;"));
    }

    #[test]
    fn handed_off_reports_are_visible_but_not_actionable() {
        let report = Report {
            id: Ulid::new(),
            kind: ReportKind::Opportunity,
            title: "Demand".into(),
            summary: "s".into(),
            severity: Severity::Medium,
            evidence: vec![],
            signal_ids: vec![Ulid::new()],
            fingerprints: vec!["f".into()],
            suspect_release: None,
            affected_count: None,
            status: ReportStatus::HandedOff,
            created_at: Utc::now(),
        };
        let html = render(&[report]);
        assert!(html.contains("data-actionable=\"false\""));
        assert!(html.contains("no PR will be generated"));
        assert!(!html.contains("Approve → dispatch"));
    }
}
