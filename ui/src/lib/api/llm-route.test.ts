import { beforeEach, describe, expect, it, vi } from "vitest";

const put = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { put, get: vi.fn() } }));

const { chosenModel, keptModels, llmRouteApi, withModel } = await import("./llm-route");

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

describe("a route's models", () => {
  it("are set and cleared in an agent's own words", () => {
    const base = { kind: "codex" as const, fallback: "wait" as const };
    const chat = withModel(base, "conversation", "gpt-6-astra");
    expect(chat).toEqual({ ...base, model_preferences: { conversation: "gpt-6-astra" } });
    const light = withModel(chat, "light", "cheap");
    expect(chosenModel(light, "light")).toBe("cheap");
    expect(chosenModel(light, "heavy")).toBeUndefined();
    expect(withModel(withModel(light, "conversation", undefined), "light", undefined)).toEqual(base);
  });

  it("are kept on the same kind and dropped on another provider's", () => {
    const codex = { kind: "codex" as const, fallback: "wait" as const, heavy_model: "gpt-6-luna" };
    expect(keptModels(codex, "codex")).toEqual({ heavy_model: "gpt-6-luna" });
    expect(keptModels(codex, "claude-code")).toEqual({});
    expect(keptModels(null, "codex")).toEqual({});
  });
});
