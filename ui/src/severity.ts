// Severity + status presentation mappings. Colors resolve to the semantic
// tokens defined in theme.css — never to literals (style-lint enforced).

import type { Report } from "./api";

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
