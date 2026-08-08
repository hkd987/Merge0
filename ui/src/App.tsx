// App shell: nav + routes + token gate. Pages are data-free until the
// token-authenticated JSON calls resolve (audit C2 model).

import { useEffect, useState } from "react";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { hasToken, onTokenChange } from "./api";
import { ToastProvider } from "./components/Toasts";
import { TokenGate } from "./components/TokenGate";
import { Dashboard } from "./pages/Dashboard";
import { Inbox } from "./pages/Inbox";
import { ReportDetail } from "./pages/ReportDetail";
import { Setup } from "./pages/Setup";

export default function App() {
  const [gateOpen, setGateOpen] = useState(!hasToken());
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(
    () =>
      onTokenChange(() => {
        if (!hasToken()) setGateOpen(true);
      }),
    [],
  );

  const unauthorized = () => setGateOpen(true);
  const gateDone = () => {
    setGateOpen(false);
    setReloadKey((k) => k + 1); // remount pages so they refetch with the token
  };

  return (
    <ToastProvider>
      <div className="shell">
        <nav className="nav" aria-label="Primary">
          <span className="wordmark">
            merge<span>0</span>
          </span>
          <NavLink to="/inbox" className={({ isActive }) => `navlink ${isActive ? "active" : ""}`}>
            Inbox
          </NavLink>
          <NavLink to="/dashboard" className={({ isActive }) => `navlink ${isActive ? "active" : ""}`}>
            Dashboard
          </NavLink>
          <NavLink to="/setup" className={({ isActive }) => `navlink ${isActive ? "active" : ""}`}>
            Setup
          </NavLink>
          <span className="spacer" />
          {hasToken() ? (
            <span className="tokenstate">
              <span className="dot" /> connected
            </span>
          ) : (
            <button className="btn ghost" onClick={() => setGateOpen(true)}>
              Set token
            </button>
          )}
        </nav>

        <Routes key={reloadKey}>
          <Route path="/" element={<Navigate to="/inbox" replace />} />
          <Route path="/inbox" element={<Inbox onUnauthorized={unauthorized} />} />
          <Route
            path="/reports/:id"
            element={<ReportDetail onUnauthorized={unauthorized} />}
          />
          <Route path="/dashboard" element={<Dashboard onUnauthorized={unauthorized} />} />
          <Route path="/setup" element={<Setup onUnauthorized={unauthorized} />} />
          <Route path="*" element={<Navigate to="/inbox" replace />} />
        </Routes>
      </div>
      {gateOpen && <TokenGate onDone={gateDone} />}
    </ToastProvider>
  );
}
