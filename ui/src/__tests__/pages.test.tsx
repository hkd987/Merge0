// Page smokes against a mocked API: the review queue renders decisions,
// inbox zero shows, the dashboard renders the gate hero.

import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { clearToken, setToken } from "../api";
import { Dashboard } from "../pages/Dashboard";
import { Inbox } from "../pages/Inbox";
import { ToastProvider } from "../components/Toasts";

const jsonResponse = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

const report = (over: Record<string, unknown>) => ({
  id: "01ABC",
  kind: "maintenance",
  title: "TypeError: districtId undefined",
  summary: "2 signal(s) from sentry + posthog correlated.",
  severity: "high",
  evidence: [
    { kind: "issue", label: "sentry issue", url: "https://sentry.example.com/1" },
  ],
  suspect_release: "v2.3.0",
  affected_count: 54,
  status: "awaiting_review",
  created_at: "2026-08-08T01:44:05Z",
  ...over,
});

afterEach(() => {
  clearToken();
  vi.restoreAllMocks();
});

describe("Inbox", () => {
  it("renders actionable cards with approve/dismiss and an opportunity card without", async () => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
      const url = String(input);
      if (url.includes("awaiting_review"))
        return Promise.resolve(jsonResponse([report({})]));
      return Promise.resolve(
        jsonResponse([
          report({ id: "01OPP", status: "handed_off", title: "Rage clicks" }),
        ]),
      );
    });
    render(
      <ToastProvider>
        <Inbox onUnauthorized={() => {}} />
      </ToastProvider>,
    );
    await waitFor(() =>
      expect(screen.getByText("TypeError: districtId undefined")).toBeInTheDocument(),
    );
    expect(screen.getByText("Approve → dispatch")).toBeInTheDocument();
    expect(screen.getByText("Opportunity")).toBeInTheDocument();
    expect(
      screen.getByText(/no PR will be generated/i),
    ).toBeInTheDocument();
  });

  it("shows inbox zero when there is nothing to review", async () => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(jsonResponse([])),
    );
    render(
      <ToastProvider>
        <Inbox onUnauthorized={() => {}} />
      </ToastProvider>,
    );
    await waitFor(() => expect(screen.getByText("Inbox zero")).toBeInTheDocument());
  });

  it("routes a 401 to the unauthorized handler", async () => {
    const onUnauthorized = vi.fn();
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("{}", { status: 401 }),
    );
    render(
      <ToastProvider>
        <Inbox onUnauthorized={onUnauthorized} />
      </ToastProvider>,
    );
    await waitFor(() => expect(onUnauthorized).toHaveBeenCalled());
  });
});

describe("Dashboard", () => {
  it("renders the gate hero and tiles from telemetry", async () => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse({
        counts: {
          window_days: 30,
          dispatched: 12,
          prs_opened: 9,
          prs_merged: 7,
          prs_closed: 1,
          prs_reverted: 1,
          runs_discarded: 3,
          reports_approved: 12,
          dismissals: { intended_behavior: 2, bad_evidence: 1 },
          median_time_to_review_secs: 480,
          tokens_on_merged: 770000,
        },
        merge_rate: 0.7778,
        runner_yield: 0.75,
        gate_precision: 0.8,
        tokens_per_merged_pr: 110000,
        phase0_gate_met: false,
      }),
    );
    render(
      <ToastProvider>
        <Dashboard onUnauthorized={() => {}} />
      </ToastProvider>,
    );
    await waitFor(() => expect(screen.getByText("78%")).toBeInTheDocument());
    expect(screen.getByText("Gate precision")).toBeInTheDocument();
    expect(screen.getByText(/9 decided/)).toBeInTheDocument();
    expect(screen.getByText("intended behavior")).toBeInTheDocument();
    expect(screen.getByText("110,000")).toBeInTheDocument();
  });
});
