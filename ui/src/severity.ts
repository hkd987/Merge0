// Severity + status presentation mappings. Colors resolve to the semantic
// tokens defined in theme.css — never to literals (style-lint enforced).

import type {
  DispatchedBy,
  FixEfficacy,
  GateConfidence,
  Report,
} from "./api";

export type Severity = Report["severity"];

export function severityToken(severity: Severity): string {
  switch (severity) {
    case "critical":
      return "var(--sev-critical)";
    case "high":
      return "var(--sev-high)";
    case "medium":
      return "var(--sev-medium)";
    case "low":
      return "var(--sev-low)";
  }
}

/** Gate confidence with the fail-conservative default: absent → "low". */
export function workOrderConfidence(
  order: { confidence?: GateConfidence | null } | null | undefined,
): GateConfidence {
  return order?.confidence ?? "low";
}

/**
 * Confidence badge tone. Positive states use `--ok`, never the accent —
 * the accent is interaction/identity only (style guide).
 */
export function confidenceToken(confidence: GateConfidence): string {
  switch (confidence) {
    case "high":
      return "var(--ok)";
    case "medium":
      return "var(--ink)";
    case "low":
      return "var(--ink-2)";
  }
}

export function confidenceLabel(confidence: GateConfidence): string {
  return `${confidence} confidence`;
}

export function efficacyLabel(efficacy: FixEfficacy): string {
  switch (efficacy) {
    case "confirmed":
      return "Fix confirmed (signals quiet)";
    case "recurred":
      return "Signals recurred after merge";
    case "pending":
      return "Pending (grace period)";
  }
}

export function efficacyToken(efficacy: FixEfficacy): string {
  switch (efficacy) {
    case "confirmed":
      return "var(--ok)";
    case "recurred":
      return "var(--danger)";
    case "pending":
      return "var(--ink-2)";
  }
}

export function dispatchedByLabel(by: DispatchedBy | string): string {
  switch (by) {
    case "auto":
      return "dispatched by autonomy dial";
    case "slack":
      return "dispatched via Slack";
    case "human":
      return "dispatched by human";
    default:
      return `dispatched by ${by}`;
  }
}

export function isOpportunity(report: Report): boolean {
  return report.status === "handed_off";
}

export function isActionable(report: Report): boolean {
  return report.status === "awaiting_review";
}

/** "2026-08-08T01:44:05.022549Z" → "2026-08-08 01:44 UTC" */
export function formatTimestamp(iso: string): string {
  const match = iso.match(/^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2})/);
  return match ? `${match[1]} ${match[2]} UTC` : iso;
}

export function formatPercent(rate: number | null): string {
  return rate === null ? "—" : `${Math.round(rate * 100)}%`;
}

export function formatCount(n: number | null | undefined): string {
  return n === null || n === undefined ? "—" : n.toLocaleString("en-US");
}

export function formatDuration(secs: number | null): string {
  if (secs === null) return "—";
  if (secs < 60) return `${secs}s`;
  const minutes = Math.round(secs / 60);
  if (minutes < 60) return `${minutes}m`;
  return `${Math.round(minutes / 6) / 10}h`;
}
