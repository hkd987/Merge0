// The auth contract: token rides every call; 401 clears it and raises.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  api,
  ApiError,
  clearToken,
  getToken,
  hasToken,
  onTokenChange,
  setToken,
} from "../api";

const jsonResponse = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });

afterEach(() => {
  clearToken();
  vi.restoreAllMocks();
});

describe("token store", () => {
  it("stores, reports, and clears the token, notifying listeners", () => {
    const events: boolean[] = [];
    const unsubscribe = onTokenChange(() => events.push(hasToken()));
    setToken("secret-token");
    expect(getToken()).toBe("secret-token");
    clearToken();
    expect(hasToken()).toBe(false);
    expect(events).toEqual([true, false]);
    unsubscribe();
  });
});

describe("api()", () => {
  it("sends the bearer token", async () => {
    setToken("secret-token");
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(jsonResponse(200, { ok: true }));
    await api("/telemetry");
    const headers = (fetchMock.mock.calls[0][1] as RequestInit)
      .headers as Record<string, string>;
    expect(headers["authorization"]).toBe("Bearer secret-token");
  });

  it("clears the token and throws ApiError(401) on unauthorized", async () => {
    setToken("stale");
    vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse(401, {}));
    await expect(api("/reports?status=awaiting_review")).rejects.toMatchObject({
      status: 401,
    });
    expect(hasToken()).toBe(false);
  });

  it("surfaces the server's error message on non-2xx", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(409, { error: "report is not awaiting review" }),
    );
    await expect(api("/reports/x/approve", { method: "POST" })).rejects.toThrow(
      "report is not awaiting review",
    );
  });

  it("wraps errors in ApiError with the status code", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse(500, {}));
    const error: unknown = await api("/telemetry").catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(500);
  });
});
