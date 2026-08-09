// Token-gate modal: replaces window.prompt(). Asks once, stores in
// localStorage, re-raised by the 401 flow in api.ts.

import { useState, type FormEvent } from "react";
import { setToken } from "../api";

export function TokenGate({ onDone }: { onDone: () => void }) {
  const [value, setValue] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setToken(value.trim());
    onDone();
  };

  return (
    <div className="backdrop" role="dialog" aria-modal="true" aria-label="API token">
      <form className="dialog" onSubmit={submit}>
        <h2>Connect to Merge0</h2>
        <p>
          Paste this server's API token (<code>MERGE0_API_TOKEN</code>). It is
          stored only in this browser and sent with each request. Leave empty
          for an unauthenticated dev server.
        </p>
        <input
          type="password"
          autoFocus
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="API token"
          aria-label="API token"
        />
        <div className="row">
          <button type="submit" className="btn">
            Connect
          </button>
        </div>
      </form>
    </div>
  );
}
