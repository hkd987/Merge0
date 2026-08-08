// Dismiss-reason dialog: the four structured reasons, keyboard 1–4,
// replaces window.prompt().

import { useEffect, useState } from "react";
import { DISMISS_REASONS } from "../api";

interface Props {
  onConfirm: (reason: string) => void;
  onCancel: () => void;
}

export function DismissDialog({ onConfirm, onCancel }: Props) {
  const [reason, setReason] = useState<string>(DISMISS_REASONS[0].value);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
      const index = Number(e.key) - 1;
      if (index >= 0 && index < DISMISS_REASONS.length) {
        onConfirm(DISMISS_REASONS[index].value);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel, onConfirm]);

  return (
    <div className="backdrop" role="dialog" aria-modal="true" aria-label="Dismiss reason">
      <div className="dialog">
        <h2>Dismiss report</h2>
        <p>
          The reason is recorded in outcome memory and steers future triage —
          “intended behavior” reroutes recurrences away from code changes.
        </p>
        <ul className="reasons">
          {DISMISS_REASONS.map((r, i) => (
            <li key={r.value}>
              <label>
                <input
                  type="radio"
                  name="reason"
                  checked={reason === r.value}
                  onChange={() => setReason(r.value)}
                />
                <kbd>{i + 1}</kbd> {r.label}
              </label>
            </li>
          ))}
        </ul>
        <div className="row">
          <button className="btn ghost" onClick={onCancel}>
            Cancel
          </button>
          <button className="btn danger" onClick={() => onConfirm(reason)}>
            Dismiss report
          </button>
        </div>
      </div>
    </div>
  );
}
