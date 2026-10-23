import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { get } }));

const { agentSettingsApi, isNotable } = await import("./agent-settings");

describe("agentSettingsApi", () => {
  beforeEach(() => get.mockClear());

  it("reads an agent's effective settings with its id encoded", () => {
    agentSettingsApi.get("data & ops");
    expect(get).toHaveBeenCalledWith("/api/agents/data%20%26%20ops/settings");
  });
});

describe("isNotable", () => {
  const row = { section: "know_how", key: "min_users", value: 2 };

  it("hides a built-in default that is in effect", () => {
    expect(isNotable({ ...row, source: "default" })).toBe(false);
  });

  it("keeps a default that holds because an override is not in effect", () => {
    expect(isNotable({ ...row, source: "default", status: "inert" })).toBe(true);
  });

  it("keeps a value another layer sets and a live fact with no layer", () => {
    expect(isNotable({ ...row, source: "agent" })).toBe(true);
    expect(isNotable(row)).toBe(true);
  });
});
