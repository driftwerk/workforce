import { beforeEach, describe, expect, it, vi } from "vitest";

const del = vi.fn(() => Promise.resolve({ removed: true }));
vi.mock("../api-client", () => ({ gateway: { del } }));

const { subscriptionsApi } = await import("./subscriptions");

describe("subscriptionsApi.removeLogin", () => {
  beforeEach(() => del.mockClear());

  it("remove encodes tier and agent_id", () => {
    subscriptionsApi.removeLogin("chatgpt", { tier: "agent_user", agent_id: "allan & co" });
    expect(del).toHaveBeenCalledWith(
      "/api/me/subscriptions/chatgpt/login?tier=agent_user&agent_id=allan+%26+co",
    );
    subscriptionsApi.removeLogin("claude", { tier: "global_user" });
    expect(del).toHaveBeenLastCalledWith("/api/me/subscriptions/claude/login?tier=global_user");
  });
});
