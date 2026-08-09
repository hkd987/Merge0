// P0-10: acceptance-rate telemetry, visible from the first PR. The merge
// rate vs the Phase 0 gate (≥60% over ≥10 decided PRs) is THE number;
// everything else supports it.

import { useCallback, useEffect, useState } from "react";
import { ApiError, fetchTelemetry, type Telemetry } from "../api";
import {
  formatCount,
  formatDuration,
  formatPercent,
} from "../severity";

const WINDOWS = [7, 30, 90] as const;
const GATE_TARGET = 0.6;

type Phase = "loading" | "ready" | "error";

export function Dashboard({ onUnauthorized }: { onUnauthorized: () => void }) {
  const [windowDays, setWindowDays] = useState<number>(30);
  const [data, setData] = useState<Telemetry | null>(null);
  const [phase, setPhase] = useState<Phase>("loading");
  const [errorText, setErrorText] = useState("");

  const load = useCallback(async () => {
    setPhase("loading");
    try {
      setData(await fetchTelemetry(windowDays));
      setPhase("ready");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return onUnauthorized();
      setErrorText(e instanceof Error ? e.message : String(e));
      setPhase("error");
    }
  }, [onUnauthorized, windowDays]);

  useEffect(() => {
    void load();
  }, [load]);

  const decided =
    data === null
      ? 0
      : data.counts.prs_merged + data.counts.prs_closed + data.counts.prs_reverted;
  const dismissals = data === null ? [] : Object.entries(data.counts.dismissals);
  const maxDismissals = Math.max(1, ...dismissals.map(([, n]) => n));

  return (
    <>
      <div className="pagehead">
        <h1>Dashboard</h1>
        <div className="seg" role="group" aria-label="Window">
          {WINDOWS.map((w) => (
            <button
              key={w}
              className={w === windowDays ? "active" : ""}
              onClick={() => setWindowDays(w)}
            >
              {w}d
            </button>
          ))}
        </div>
      </div>

      {phase === "loading" && (
        <div className="stack" aria-label="Loading telemetry">
          <div className="skeleton" />
          <div className="skeleton" />
        </div>
      )}

      {phase === "error" && (
        <div className="errorbox" role="alert">
          <span>Couldn't load telemetry: {errorText}</span>
          <button className="btn ghost" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === "ready" && data !== null && (
        <>
          <div className="tiles">
            <div className="tile wide">
              <div className="label">
                Merge rate — Phase 0 gate: ≥60% over ≥10 decided PRs
              </div>
              <div className="value num">
                {formatPercent(data.merge_rate)}
                <span className="sub" style={{ display: "inline", marginLeft: "var(--s3)" }}>
                  {decided} decided · {data.phase0_gate_met ? "gate MET" : "gate not met yet"}
                </span>
              </div>
              <div className="gatebar" aria-hidden="true">
                <div
                  className={`fill ${data.phase0_gate_met ? "met" : ""}`}
                  style={{
                    width: `${Math.min(100, ((data.merge_rate ?? 0) / GATE_TARGET) * 100)}%`,
                  }}
                />
              </div>
            </div>

            <Stat label="Work orders dispatched" value={formatCount(data.counts.dispatched)} />
            <Stat label="PRs opened" value={formatCount(data.counts.prs_opened)} />
            <Stat label="PRs merged" value={formatCount(data.counts.prs_merged)} />
            <Stat label="PRs closed" value={formatCount(data.counts.prs_closed)} />
            <Stat label="PRs reverted" value={formatCount(data.counts.prs_reverted)} />
            <Stat label="Runs self-discarded" value={formatCount(data.counts.runs_discarded)} />
            <Stat label="Runner yield" value={formatPercent(data.runner_yield)} sub="PRs opened / dispatched" />
            <Stat label="Gate precision" value={formatPercent(data.gate_precision)} sub="approved / (approved + dismissed)" />
            <Stat
              label="Tokens per merged PR"
              value={
                data.tokens_per_merged_pr === null
                  ? "—"
                  : Math.round(data.tokens_per_merged_pr).toLocaleString("en-US")
              }
            />
            <Stat
              label="Median time to review"
              value={formatDuration(data.counts.median_time_to_review_secs)}
              sub="target < 10m"
            />
            <Stat
              label="Fix efficacy"
              value={formatPercent(data.fix_efficacy_rate)}
              sub={`confirmed ${data.counts.fixes_confirmed} / recurred ${data.counts.fixes_recurred} / pending ${data.counts.fixes_pending}`}
            />
            <Stat
              label="Auto-dispatched"
              value={formatCount(data.counts.auto_dispatched)}
              sub="autonomy dial"
            />
            <Stat
              label="Tokens (24h)"
              value={formatCount(data.counts.tokens_spent_24h)}
              sub="gate + runner spend"
            />
          </div>

          <div className="section">
            <h2>Dismissals by reason</h2>
            {dismissals.length === 0 ? (
              <div className="empty">No dismissals in this window.</div>
            ) : (
              <div className="stack">
                {dismissals.map(([reason, count]) => (
                  <div className="hbar" key={reason}>
                    <span className="mono muted">{reason.replace("_", " ")}</span>
                    <div className="track">
                      <div
                        className="fill"
                        style={{ width: `${(count / maxDismissals) * 100}%` }}
                      />
                    </div>
                    <span className="num mono">{count}</span>
                  </div>
                ))}
              </div>
            )}
          </div>
        </>
      )}
    </>
  );
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="tile">
      <div className="label">{label}</div>
      <div className="value num">{value}</div>
      {sub !== undefined && <div className="sub">{sub}</div>}
    </div>
  );
}
