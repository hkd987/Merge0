// Report detail: everything the loop knows about one report — the gate's
// decision + confidence, the dispatch audit trail, and (post-merge) whether
// the fix actually made the signals stop. The inbox list payload carries no
// work order, so confidence surfaces here rather than via N+1 list fetches.

import { useCallback, useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { ApiError, fetchReportDetail, type ReportDetail as Detail } from "../api";
import {
  confidenceLabel,
  confidenceToken,
  dispatchedByLabel,
  efficacyLabel,
  efficacyToken,
  formatCount,
  formatTimestamp,
  severityToken,
  workOrderConfidence,
} from "../severity";

type Phase = "loading" | "ready" | "error";

export function ReportDetail({ onUnauthorized }: { onUnauthorized: () => void }) {
  const { id = "" } = useParams();
  const [detail, setDetail] = useState<Detail | null>(null);
  const [phase, setPhase] = useState<Phase>("loading");
  const [errorText, setErrorText] = useState("");

  const load = useCallback(async () => {
    setPhase("loading");
    try {
      setDetail(await fetchReportDetail(id));
      setPhase("ready");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return onUnauthorized();
      setErrorText(e instanceof Error ? e.message : String(e));
      setPhase("error");
    }
  }, [id, onUnauthorized]);

  useEffect(() => {
    void load();
  }, [load]);

  const confidence =
    detail?.work_order == null ? null : workOrderConfidence(detail.work_order);

  // Tracker delivery: present in story-only mode (no dispatch, report
  // handed off) and in accompany mode (story + dispatch). Absent — not
  // just null — on reports that predate tracker delivery, hence `?? null`.
  const storyUrl = detail?.story_url ?? null;

  return (
    <>
      <div className="pagehead">
        <h1>Report</h1>
        <Link to="/inbox" className="mono" style={{ fontSize: "var(--fs-small)" }}>
          ← Inbox
        </Link>
      </div>

      {phase === "loading" && (
        <div className="stack" aria-label="Loading report">
          <div className="skeleton" />
          <div className="skeleton" />
        </div>
      )}

      {phase === "error" && (
        <div className="errorbox" role="alert">
          <span>Couldn't load report: {errorText}</span>
          <button className="btn ghost" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === "ready" && detail !== null && (
        <>
          <article
            className="card"
            style={{ "--stripe": severityToken(detail.report.severity) } as React.CSSProperties}
            aria-label={detail.report.title}
          >
            <div className="cardhead">
              <span
                className="badge"
                style={
                  {
                    "--badge-color": severityToken(detail.report.severity),
                  } as React.CSSProperties
                }
              >
                {detail.report.severity}
              </span>
              <span className="title" title={detail.report.title}>
                {detail.report.title}
              </span>
              <span className="mono muted num" style={{ fontSize: "var(--fs-label)" }}>
                {formatTimestamp(detail.report.created_at)}
              </span>
            </div>
            <div className="meta num">
              <span>affected {formatCount(detail.report.affected_count)}</span>
              <span>release {detail.report.suspect_release ?? "—"}</span>
              <span>status {detail.report.status.replace(/_/g, " ")}</span>
            </div>
            <p className="summary" style={{ WebkitLineClamp: "unset" }}>
              {detail.report.summary}
            </p>
            {detail.report.evidence.length > 0 && (
              <div className="chips">
                {detail.report.evidence.map((e) => (
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
          </article>

          {detail.gate_decision !== null && (
            <div className="section">
              <h2>Gate decision</h2>
              <div className="card">
                <div className="cardhead">
                  {detail.gate_decision.decision === "work" ? (
                    <>
                      <span className="badge">Work order</span>
                      {confidence !== null && (
                        <span
                          className="badge"
                          style={
                            {
                              "--badge-color": confidenceToken(confidence),
                            } as React.CSSProperties
                          }
                        >
                          {confidenceLabel(confidence)}
                        </span>
                      )}
                    </>
                  ) : (
                    <span className="badge">Skipped</span>
                  )}
                </div>
                {detail.gate_decision.decision === "skip" && (
                  <p className="summary" style={{ WebkitLineClamp: "unset" }}>
                    {detail.gate_decision.reason}
                  </p>
                )}
                {detail.work_order !== null && (
                  <>
                    <p className="summary" style={{ WebkitLineClamp: "unset" }}>
                      {detail.work_order.summary}
                    </p>
                    <div className="meta">
                      <span>repo {detail.work_order.repo}</span>
                      <span>success: {detail.work_order.success_criteria}</span>
                    </div>
                  </>
                )}
              </div>
            </div>
          )}

          {detail.dispatch !== null && (
            <div className="section">
              <h2>Dispatch</h2>
              <div className="card">
                <div className="meta num" style={{ marginTop: 0 }}>
                  <span>runner {detail.dispatch.runner_kind}</span>
                  <span>status {detail.dispatch.status.replace(/_/g, " ")}</span>
                  <span>{dispatchedByLabel(detail.dispatch.dispatched_by)}</span>
                  <span>{formatTimestamp(detail.dispatch.dispatched_at)}</span>
                  {detail.dispatch.tokens_spent !== null && (
                    <span>tokens {formatCount(detail.dispatch.tokens_spent)}</span>
                  )}
                </div>
                {detail.dispatch.pr_url !== null && (
                  <div className="chips">
                    <a
                      className="chip"
                      href={detail.dispatch.pr_url}
                      target="_blank"
                      rel="noopener noreferrer"
                    >
                      pull request ↗
                    </a>
                  </div>
                )}
                {detail.dispatch.discard_reason !== null && (
                  <p className="summary" style={{ WebkitLineClamp: "unset" }}>
                    Discarded: {detail.dispatch.discard_reason}
                  </p>
                )}
              </div>
            </div>
          )}

          {storyUrl !== null && (
            <div className="section">
              <h2>Tracker story</h2>
              <div className="card">
                <div className="meta" style={{ marginTop: 0 }}>
                  <span>
                    {detail.dispatch === null
                      ? "Filed to the tracker — handed off, no PR."
                      : "Filed to the tracker alongside the dispatched PR."}
                  </span>
                </div>
                <div className="chips">
                  <a
                    className="chip"
                    href={storyUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    {detail.story_key ?? "tracker story"} ↗
                  </a>
                </div>
              </div>
            </div>
          )}

          {detail.fix_efficacy !== null && (
            <div className="section">
              <h2>Fix efficacy</h2>
              <p
                className="mono"
                style={{
                  color: efficacyToken(detail.fix_efficacy),
                  fontSize: "var(--fs-small)",
                  margin: 0,
                }}
              >
                {efficacyLabel(detail.fix_efficacy)}
              </p>
            </div>
          )}

          {detail.handoff_brief !== null && (
            <div className="section">
              <h2>Handoff brief</h2>
              <p className="muted" style={{ margin: 0, fontSize: "var(--fs-small)" }}>
                {detail.handoff_brief}
              </p>
            </div>
          )}
        </>
      )}
    </>
  );
}
