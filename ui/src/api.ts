// API layer: token storage + authenticated fetch + typed endpoints.
// Auth model (audit C2): pages are data-free; the token lives in
// localStorage and rides every JSON call. A 401 clears it and notifies
// subscribers so the app can raise the token-gate modal.

const TOKEN_KEY = "merge0_token";

type TokenListener = () => void;
const listeners = new Set<TokenListener>();

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
  listeners.forEach((fn) => fn());
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
  listeners.forEach((fn) => fn());
}

export function hasToken(): boolean {
  return getToken() !== null;
}

export function onTokenChange(fn: TokenListener): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    ...((init?.headers as Record<string, string>) ?? {}),
  };
  const token = getToken();
  if (token) headers["authorization"] = `Bearer ${token}`;
  const res = await fetch(path, { ...init, headers });
  if (res.status === 401) {
    clearToken();
    throw new ApiError(401, "Unauthorized — enter your API token.");
  }
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    const detail =
      typeof body === "object" && body !== null && "error" in body
        ? String((body as { error: unknown }).error)
        : res.statusText;
    throw new ApiError(res.status, detail);
  }
  return body as T;
}

// ---- types mirroring the server's JSON ----

export interface EvidenceLink {
  kind: string;
  label: string;
  url: string;
}

export interface Report {
  id: string;
  kind: string;
  title: string;
  summary: string;
  severity: "low" | "medium" | "high" | "critical";
  evidence: EvidenceLink[];
  suspect_release: string | null;
  affected_count: number | null;
  status: string;
  created_at: string;
}

/** The gate's self-assessed fix confidence. Absent on old data → "low". */
export type GateConfidence = "low" | "medium" | "high";

/** Post-merge verdict on one fix. null until the PR merges. */
export type FixEfficacy = "pending" | "confirmed" | "recurred";

/** Who pulled the dispatch trigger (the autonomy dial's audit trail). */
export type DispatchedBy = "human" | "slack" | "auto";

export interface WorkOrder {
  report_id: string;
  repo: string;
  summary: string;
  evidence: EvidenceLink[];
  repro: string;
  suspect_change?: string | null;
  success_criteria: string;
  constraints: string;
  /** May be absent on Work Orders stored before the field existed. */
  confidence?: GateConfidence;
}

export type GateDecision =
  | { decision: "work"; work_order: WorkOrder }
  | { decision: "skip"; reason: string };

export interface Dispatch {
  runner_kind: string;
  dispatched_at: string;
  status: string;
  dispatched_by: DispatchedBy;
  pr_url: string | null;
  branch: string | null;
  discard_reason: string | null;
  diagnosis: string | null;
  tokens_spent: number | null;
}

export interface OutcomeRef {
  work_order_id: string;
  outcome: "merged" | "closed" | "reverted" | "discarded";
  occurred_at: string;
  note?: string | null;
}

/** GET /reports/{id} — the report plus everything the loop knows about it. */
export interface ReportDetail {
  report: Report;
  gate_decision: GateDecision | null;
  work_order: WorkOrder | null;
  dispatch: Dispatch | null;
  outcomes: OutcomeRef[];
  handoff_brief: string | null;
  fix_efficacy: FixEfficacy | null;
}

export interface TelemetryCounts {
  window_days: number;
  dispatched: number;
  prs_opened: number;
  prs_merged: number;
  prs_closed: number;
  prs_reverted: number;
  runs_discarded: number;
  reports_approved: number;
  dismissals: Record<string, number>;
  median_time_to_review_secs: number | null;
  tokens_on_merged: number | null;
  fixes_confirmed: number;
  fixes_recurred: number;
  fixes_pending: number;
  auto_dispatched: number;
  tokens_spent_24h: number;
}

export interface Telemetry {
  counts: TelemetryCounts;
  merge_rate: number | null;
  runner_yield: number | null;
  gate_precision: number | null;
  tokens_per_merged_pr: number | null;
  /** confirmed / (confirmed + recurred); null until a fix leaves grace. */
  fix_efficacy_rate: number | null;
  phase0_gate_met: boolean;
}

export interface Onboarding {
  repo: string;
  files: Record<string, string>;
  secrets_to_configure: Record<string, string>;
  checklist: string[];
  safety: { satisfied: boolean; failures: string[] };
}

export const DISMISS_REASONS = [
  { value: "intended_behavior", label: "Intended behavior" },
  { value: "wont_fix", label: "Won't fix" },
  { value: "duplicate", label: "Duplicate" },
  { value: "bad_evidence", label: "Bad evidence" },
] as const;

export const fetchReports = (status: string) =>
  api<Report[]>(`/reports?status=${status}`);

export const fetchReportDetail = (id: string) =>
  api<ReportDetail>(`/reports/${id}`);

export const approveReport = (id: string) =>
  api<{ approved: string; dispatched_to: string }>(`/reports/${id}/approve`, {
    method: "POST",
  });

export const dismissReport = (id: string, reason: string) =>
  api<{ dismissed: string }>(`/reports/${id}/dismiss`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });

export const fetchTelemetry = (windowDays: number) =>
  api<Telemetry>(`/telemetry?window_days=${windowDays}`);

export const fetchOnboarding = () => api<Onboarding>("/onboarding");
