import { describe, expect, it } from "vitest";
import {
  confidenceLabel,
  confidenceToken,
  dispatchedByLabel,
  efficacyLabel,
  efficacyToken,
  formatCount,
  formatDuration,
  formatPercent,
  formatTimestamp,
  severityToken,
  workOrderConfidence,
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

describe("gate confidence", () => {
  it("treats a missing confidence as low (fail-conservative)", () => {
    expect(workOrderConfidence(null)).toBe("low");
    expect(workOrderConfidence(undefined)).toBe("low");
    expect(workOrderConfidence({})).toBe("low");
    expect(workOrderConfidence({ confidence: null })).toBe("low");
    expect(workOrderConfidence({ confidence: "high" })).toBe("high");
    expect(workOrderConfidence({ confidence: "medium" })).toBe("medium");
  });

  it("maps confidence to tokens — positive is --ok, never the accent", () => {
    expect(confidenceToken("high")).toBe("var(--ok)");
    expect(confidenceToken("medium")).toBe("var(--ink)");
    expect(confidenceToken("low")).toBe("var(--ink-2)");
  });

  it("labels confidence in product language", () => {
    expect(confidenceLabel("high")).toBe("high confidence");
    expect(confidenceLabel("low")).toBe("low confidence");
  });
});

describe("fix efficacy", () => {
  it("labels each verdict in product language", () => {
    expect(efficacyLabel("confirmed")).toBe("Fix confirmed (signals quiet)");
    expect(efficacyLabel("recurred")).toBe("Signals recurred after merge");
    expect(efficacyLabel("pending")).toBe("Pending (grace period)");
  });

  it("maps verdicts to semantic tokens", () => {
    expect(efficacyToken("confirmed")).toBe("var(--ok)");
    expect(efficacyToken("recurred")).toBe("var(--danger)");
    expect(efficacyToken("pending")).toBe("var(--ink-2)");
  });
});

describe("dispatched-by", () => {
  it("names the trigger-puller, autonomy dial included", () => {
    expect(dispatchedByLabel("auto")).toBe("dispatched by autonomy dial");
    expect(dispatchedByLabel("slack")).toBe("dispatched via Slack");
    expect(dispatchedByLabel("human")).toBe("dispatched by human");
    expect(dispatchedByLabel("cron")).toBe("dispatched by cron");
  });
});
