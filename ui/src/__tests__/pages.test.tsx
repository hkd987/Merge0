// Page smokes against a mocked API: the review queue renders decisions,
// inbox zero shows, the dashboard renders the gate hero, and the report
// detail surfaces gate confidence + dispatch audit + fix efficacy.

import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { clearToken, setToken } from "../api";
import { Dashboard } from "../pages/Dashboard";
import { Inbox } from "../pages/Inbox";
import { ReportDetail } from "../pages/ReportDetail";
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

const detailResponse = (over: Record<string, unknown>) => ({
  report: report({}),
  gate_decision: {
    decision: "work",
    work_order: workOrder({}),
  },
  work_order: workOrder({}),
  dispatch: null,
  outcomes: [],
  handoff_brief: null,
  fix_efficacy: null,
  ...over,
});

const workOrder = (over: Record<string, unknown>) => ({
  report_id: "01ABC",
  repo: "example/app",
  summary: "Guard districtId before dereferencing.",
  evidence: [],
  repro: "Open the sync panel with no district.",
  success_criteria: "No TypeError in SyncStatusPanel.",
  constraints: "Small scoped diff.",
  confidence: "high",
  ...over,
});

const renderDetail = () =>
  render(
    <ToastProvider>
      <MemoryRouter initialEntries={["/inbox/01ABC"]}>
        <Routes>
          <Route
            path="/inbox/:id"
            element={<ReportDetail onUnauthorized={() => {}} />}
          />
        </Routes>
      </MemoryRouter>
    </ToastProvider>,
  );

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
        <MemoryRouter>
          <Inbox onUnauthorized={() => {}} />
        </MemoryRouter>
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
        <MemoryRouter>
          <Inbox onUnauthorized={() => {}} />
        </MemoryRouter>
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
        <MemoryRouter>
          <Inbox onUnauthorized={onUnauthorized} />
        </MemoryRouter>
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
          fixes_confirmed: 4,
          fixes_recurred: 1,
          fixes_pending: 2,
          auto_dispatched: 5,
          tokens_spent_24h: 250000,
        },
        merge_rate: 0.7778,
        runner_yield: 0.75,
        gate_precision: 0.8,
        tokens_per_merged_pr: 110000,
        fix_efficacy_rate: 0.8,
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
    // Close-the-loop tiles.
    expect(screen.getByText("Fix efficacy")).toBeInTheDocument();
    expect(
      screen.getByText("confirmed 4 / recurred 1 / pending 2"),
    ).toBeInTheDocument();
    expect(screen.getByText("Auto-dispatched")).toBeInTheDocument();
    expect(screen.getByText("5")).toBeInTheDocument();
    expect(screen.getByText("Tokens (24h)")).toBeInTheDocument();
    expect(screen.getByText("250,000")).toBeInTheDocument();
  });
});

describe("ReportDetail", () => {
  it("shows gate confidence next to the decision and the dispatch trail", async () => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(
        detailResponse({
          dispatch: {
            runner_kind: "github_actions",
            dispatched_at: "2026-08-08T02:00:00Z",
            status: "pr_open",
            dispatched_by: "auto",
            pr_url: "https://github.com/example/app/pull/7",
            branch: "merge0/fix",
            discard_reason: null,
            diagnosis: null,
            tokens_spent: 95000,
          },
        }),
      ),
    );
    renderDetail();
    await waitFor(() =>
      expect(screen.getByText("high confidence")).toBeInTheDocument(),
    );
    expect(screen.getByText("Work order")).toBeInTheDocument();
    expect(screen.getByText("dispatched by autonomy dial")).toBeInTheDocument();
    expect(screen.getByText(/pull request/)).toBeInTheDocument();
  });

  it("treats a missing confidence as low", async () => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(
        detailResponse({
          gate_decision: {
            decision: "work",
            work_order: workOrder({ confidence: undefined }),
          },
          work_order: workOrder({ confidence: undefined }),
        }),
      ),
    );
    renderDetail();
    await waitFor(() =>
      expect(screen.getByText("low confidence")).toBeInTheDocument(),
    );
  });

  it.each([
    ["confirmed", "Fix confirmed (signals quiet)"],
    ["recurred", "Signals recurred after merge"],
    ["pending", "Pending (grace period)"],
  ])("renders the %s fix-efficacy verdict", async (verdict, label) => {
    setToken("t");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(detailResponse({ fix_efficacy: verdict })),
    );
    renderDetail();
    await waitFor(() => expect(screen.getByText(label)).toBeInTheDocument());
  });

  it("routes a 401 to the unauthorized handler", async () => {
    const onUnauthorized = vi.fn();
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("{}", { status: 401 }),
    );
    render(
      <ToastProvider>
        <MemoryRouter initialEntries={["/inbox/01ABC"]}>
          <Routes>
            <Route
              path="/inbox/:id"
              element={<ReportDetail onUnauthorized={onUnauthorized} />}
            />
          </Routes>
        </MemoryRouter>
      </ToastProvider>,
    );
    await waitFor(() => expect(onUnauthorized).toHaveBeenCalled());
  });
});
