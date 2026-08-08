import { describe, expect, it } from "vitest";
import {
  formatCount,
  formatDuration,
  formatPercent,
  formatTimestamp,
  severityToken,
} from "../severity";

describe("severity + formatting", () => {
  it("maps every severity to its semantic token (never the accent)", () => {
    expect(severityToken("critical")).toBe("var(--sev-critical)");
    expect(severityToken("high")).toBe("var(--sev-high)");
    expect(severityToken("medium")).toBe("var(--sev-medium)");
    expect(severityToken("low")).toBe("var(--sev-low)");
  });

  it("formats timestamps to minute precision UTC", () => {
    expect(formatTimestamp("2026-08-08T01:44:05.022549Z")).toBe(
      "2026-08-08 01:44 UTC",
    );
    expect(formatTimestamp("garbage")).toBe("garbage");
  });

  it("formats rates, counts, and durations with em-dash nulls", () => {
    expect(formatPercent(0.667)).toBe("67%");
    expect(formatPercent(null)).toBe("—");
    expect(formatCount(12345)).toBe("12,345");
    expect(formatCount(null)).toBe("—");
    expect(formatDuration(45)).toBe("45s");
    expect(formatDuration(600)).toBe("10m");
    expect(formatDuration(5400)).toBe("1.5h");
    expect(formatDuration(null)).toBe("—");
  });
});
