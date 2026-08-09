// Setup: the onboarding bundle (§6a) — safety status, checklist, and the
// three files to commit, each copyable.

import { useCallback, useEffect, useState } from "react";
import { ApiError, fetchOnboarding, type Onboarding } from "../api";
import { useToasts } from "../components/Toasts";

type Phase = "loading" | "ready" | "error";

export function Setup({ onUnauthorized }: { onUnauthorized: () => void }) {
  const [data, setData] = useState<Onboarding | null>(null);
  const [phase, setPhase] = useState<Phase>("loading");
  const [errorText, setErrorText] = useState("");
  const { toast, toastError } = useToasts();

  const load = useCallback(async () => {
    setPhase("loading");
    try {
      setData(await fetchOnboarding());
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

  const copy = async (name: string, text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast(`Copied ${name}`);
    } catch {
      toastError("Copy failed — select the text and copy manually.");
    }
  };

  return (
    <>
      <div className="pagehead">
        <h1>Setup</h1>
        {phase === "ready" && data !== null && (
          <span className="label mono">{data.repo}</span>
        )}
      </div>

      {phase === "loading" && (
        <div className="stack" aria-label="Loading setup bundle">
          <div className="skeleton" />
          <div className="skeleton" />
        </div>
      )}

      {phase === "error" && (
        <div className="errorbox" role="alert">
          <span>Couldn't load the setup bundle: {errorText}</span>
          <button className="btn ghost" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === "ready" && data !== null && (
        <>
          <div className="tiles">
            <div className="tile wide">
              <div className="label">Repo safety (branch protection + required checks)</div>
              <div className="value num" style={{ color: data.safety.satisfied ? "var(--ok)" : "var(--danger)" }}>
                {data.safety.satisfied ? "Verified" : "Not verified"}
              </div>
              {!data.safety.satisfied && (
                <div className="sub">
                  {data.safety.failures.join(" · ")} — Merge0 refuses to dispatch until this is fixed.
                </div>
              )}
            </div>
          </div>

          <div className="section">
            <h2>Checklist</h2>
            <ol className="checklist">
              {data.checklist.map((item) => (
                <li key={item}>{item}</li>
              ))}
            </ol>
          </div>

          <div className="section">
            <h2>Actions secrets to configure</h2>
            <div className="stack">
              {Object.entries(data.secrets_to_configure).map(([name, why]) => (
                <div className="hbar" key={name} style={{ gridTemplateColumns: "16rem 1fr" }}>
                  <code>{name}</code>
                  <span className="muted" style={{ fontSize: "var(--fs-small)" }}>{why}</span>
                </div>
              ))}
            </div>
          </div>

          <div className="section">
            <h2>Files to commit</h2>
            <div className="stack">
              {Object.entries(data.files).map(([path, content]) => (
                <div className="filecard" key={path}>
                  <div className="filehead">
                    <span>{path}</span>
                    <button className="btn ghost" onClick={() => void copy(path, content)}>
                      Copy
                    </button>
                  </div>
                  <pre>{content}</pre>
                </div>
              ))}
            </div>
          </div>
        </>
      )}
    </>
  );
}
