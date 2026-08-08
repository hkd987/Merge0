// A report card: one-screen decision (PRD §6) — severity stripe, title,
// meta, summary, evidence chips above the fold, actions below.

import { Link } from "react-router-dom";
import type { Report } from "../api";
import {
  formatCount,
  formatTimestamp,
  isActionable,
  isOpportunity,
  severityToken,
} from "../severity";

interface Props {
  report: Report;
  focused: boolean;
  busy: boolean;
  onApprove: (id: string) => void;
  onDismiss: (id: string) => void;
}

export function ReportCard({ report, focused, busy, onApprove, onDismiss }: Props) {
  const opportunity = isOpportunity(report);
  const stripe = opportunity ? "var(--ink-2)" : severityToken(report.severity);
  return (
    <article
      className={`card ${focused ? "focused" : ""}`}
      style={{ "--stripe": stripe } as React.CSSProperties}
      data-report-id={report.id}
      aria-label={report.title}
    >
      <div className="cardhead">
        {opportunity ? (
          <span className="badge">Opportunity</span>
        ) : (
          <span
            className="badge"
            style={{ "--badge-color": stripe } as React.CSSProperties}
          >
            {report.severity}
          </span>
        )}
        <Link className="title" title={report.title} to={`/inbox/${report.id}`}>
          {report.title}
        </Link>
        <span className="mono muted num" style={{ fontSize: "var(--fs-label)" }}>
          {formatTimestamp(report.created_at)}
        </span>
      </div>
      <div className="meta num">
        <span>affected {formatCount(report.affected_count)}</span>
        <span>release {report.suspect_release ?? "—"}</span>
      </div>
      <p className="summary">{report.summary}</p>
      {report.evidence.length > 0 && (
        <div className="chips">
          {report.evidence.map((e) => (
            <a
              key={e.url}
              className="chip"
              href={e.url}
              target="_blank"
              rel="noopener noreferrer"
            >
              {e.label} ↗
            </a>
          ))}
        </div>
      )}
      {opportunity && (
        <p className="muted" style={{ margin: "var(--s3) 0 0", fontSize: "var(--fs-small)" }}>
          Handed off for a human decision — no PR will be generated.
        </p>
      )}
      {isActionable(report) && (
        <div className="actions">
          <button className="btn" disabled={busy} onClick={() => onApprove(report.id)}>
            Approve → dispatch
          </button>
          <button className="btn ghost" disabled={busy} onClick={() => onDismiss(report.id)}>
            Dismiss…
          </button>
        </div>
      )}
    </article>
  );
}
