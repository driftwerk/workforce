import { beforeEach, describe, expect, it, vi } from "vitest";

const put = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { put, get: vi.fn() } }));

const { llmRouteApi } = await import("./llm-route");

describe("llmRouteApi", () => {
  beforeEach(() => put.mockClear());

  it("sends the route under its key, null clearing it", () => {
    llmRouteApi.setMine({ kind: "claude-code", fallback: "hub" });
    expect(put).toHaveBeenLastCalledWith("/api/me/llm-route", {
      route: { kind: "claude-code", fallback: "hub" },
    });
    llmRouteApi.setMine(null);
    expect(put).toHaveBeenLastCalledWith("/api/me/llm-route", { route: null });
  });

  it("encodes the agent id and sends hub as a word", () => {
    llmRouteApi.setAgent("allan & co", "hub");
    expect(put).toHaveBeenLastCalledWith("/api/me/agents/allan%20%26%20co/llm-route", { route: "hub" });
  });
});
