// The review queue (PRD §6): newest-first, one-screen decision per report,
// keyboard-actionable, no configuration surfaces. Anything that grows
// median time-to-review is a regression.

import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { ApiError, approveReport, dismissReport, fetchReports, type Report } from "../api";
import { DismissDialog } from "../components/DismissDialog";
import { ReportCard } from "../components/ReportCard";
import { useToasts } from "../components/Toasts";
import { isActionable } from "../severity";

type Phase = "loading" | "ready" | "error";

export function Inbox({ onUnauthorized }: { onUnauthorized: () => void }) {
  const [reports, setReports] = useState<Report[]>([]);
  const [phase, setPhase] = useState<Phase>("loading");
  const [errorText, setErrorText] = useState("");
  const [focus, setFocus] = useState(0);
  const [busy, setBusy] = useState(false);
  const [dismissing, setDismissing] = useState<string | null>(null);
  const { toast, toastError } = useToasts();
  const navigate = useNavigate();
  const dismissingRef = useRef(dismissing);
  dismissingRef.current = dismissing;

  const load = useCallback(async () => {
    try {
      const [awaiting, handed] = await Promise.all([
        fetchReports("awaiting_review"),
        fetchReports("handed_off"),
      ]);
      const all = [...awaiting, ...handed].sort((a, b) =>
        b.created_at.localeCompare(a.created_at),
      );
      setReports(all);
      setFocus((f) => Math.min(f, Math.max(all.length - 1, 0)));
      setPhase("ready");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return onUnauthorized();
      setErrorText(e instanceof Error ? e.message : String(e));
      setPhase("error");
    }
  }, [onUnauthorized]);

  useEffect(() => {
    void load();
  }, [load]);

  const approve = useCallback(
    async (id: string) => {
      setBusy(true);
      try {
        const res = await approveReport(id);
        toast(`Approved — dispatched to ${res.dispatched_to}`);
        await load();
      } catch (e) {
        if (e instanceof ApiError && e.status === 401) return onUnauthorized();
        toastError(
          `Approve failed: ${e instanceof Error ? e.message : e}. Fix the cause and retry.`,
        );
      } finally {
        setBusy(false);
      }
    },
    [load, onUnauthorized, toast, toastError],
  );

  const dismiss = useCallback(
    async (id: string, reason: string) => {
      setDismissing(null);
      setBusy(true);
      try {
        await dismissReport(id, reason);
        toast(`Dismissed (${reason.replace("_", " ")})`);
        await load();
      } catch (e) {
        if (e instanceof ApiError && e.status === 401) return onUnauthorized();
        toastError(
          `Dismiss failed: ${e instanceof Error ? e.message : e}. Fix the cause and retry.`,
        );
      } finally {
        setBusy(false);
      }
    },
    [load, onUnauthorized, toast, toastError],
  );

  // Keyboard flow: j/k move, o opens detail, a approve, d opens the reason
  // dialog (which itself handles 1–4/Escape).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (dismissingRef.current !== null) return; // dialog owns the keyboard
      if (e.target instanceof HTMLInputElement) return;
      if (e.key === "j") setFocus((f) => Math.min(f + 1, reports.length - 1));
      if (e.key === "k") setFocus((f) => Math.max(f - 1, 0));
      const focusedReport = reports[focus];
      if (!focusedReport) return;
      if (e.key === "o") navigate(`/inbox/${focusedReport.id}`);
      if (!isActionable(focusedReport)) return;
      if (e.key === "a") void approve(focusedReport.id);
      if (e.key === "d") setDismissing(focusedReport.id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [approve, focus, navigate, reports]);

  useEffect(() => {
    document
      .querySelectorAll(".card")
      [focus]?.scrollIntoView({ block: "nearest" });
  }, [focus]);

  return (
    <>
      <div className="pagehead">
        <h1>Inbox</h1>
        {phase === "ready" && (
          <span className="label num">{reports.length} report(s)</span>
        )}
      </div>

      {phase === "loading" && (
        <div className="stack" aria-label="Loading reports">
          <div className="skeleton" />
          <div className="skeleton" />
          <div className="skeleton" />
        </div>
      )}

      {phase === "error" && (
        <div className="errorbox" role="alert">
          <span>Couldn't load reports: {errorText}</span>
          <button className="btn ghost" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === "ready" && reports.length === 0 && (
        <div className="empty">
          <div className="big">Inbox zero</div>
          A quiet inbox that's always right.
        </div>
      )}

      {phase === "ready" && reports.length > 0 && (
        <div className="stack">
          {reports.map((report, i) => (
            <ReportCard
              key={report.id}
              report={report}
              focused={i === focus}
              busy={busy}
              onApprove={(id) => void approve(id)}
              onDismiss={(id) => setDismissing(id)}
            />
          ))}
        </div>
      )}

      <div className="kbdbar" aria-hidden="true">
        <span>
          <kbd>j</kbd>/<kbd>k</kbd> move
        </span>
        <span>
          <kbd>o</kbd> open
        </span>
        <span>
          <kbd>a</kbd> approve
        </span>
        <span>
          <kbd>d</kbd> dismiss
        </span>
        <span>
          <kbd>1</kbd>–<kbd>4</kbd> reason
        </span>
      </div>

      {dismissing !== null && (
        <DismissDialog
          onCancel={() => setDismissing(null)}
          onConfirm={(reason) => void dismiss(dismissing, reason)}
        />
      )}
    </>
  );
}
