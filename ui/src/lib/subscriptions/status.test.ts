import { describe, expect, it } from "vitest";
import type { LoginSlotView } from "@/lib/api/subscriptions";
import { loginStatusLabel, ownLoginStatus } from "./status";

const now = new Date(2026, 9, 1, 9, 30);
const time = (at: Date) => at.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
const date = (at: Date) => at.toLocaleDateString(undefined, { day: "numeric", month: "short" });

describe("loginStatusLabel", () => {
  it("cooling label shows local time and date when not today", () => {
    const later = new Date(2026, 9, 1, 14, 0);
    expect(loginStatusLabel({ state: "cooling", until: later.toISOString() }, now)).toBe(
      `At limit until ${time(later)}`,
    );
    const friday = new Date(2026, 9, 3, 14, 0);
    expect(loginStatusLabel({ state: "cooling", until: friday.toISOString() }, now)).toBe(
      `At limit until ${date(friday)} ${time(friday)}`,
    );
  });

  it("invalid reads reconnect", () => {
    expect(loginStatusLabel({ state: "invalid", reason: "refresh_token_reused" }, now)).toBe(
      "Signed out: reconnect",
    );
  });
});

describe("ownLoginStatus", () => {
  const slot = (tier: LoginSlotView["tier"], agent_id: string | null, state: "connected" | "not_set"): LoginSlotView => ({
    kind: "chatgpt",
    tier,
    agent_id,
    agent_name: null,
    status: state === "connected" ? { state, account: null } : { state },
    updated_at: null,
    updated_by: null,
  });
  const logins = [slot("global_user", null, "not_set"), slot("agent_user", "allan", "connected")];

  it("reads the caller's own login on one agent apart from their global one", () => {
    expect(ownLoginStatus(logins, "chatgpt", "agent_user", "allan").state).toBe("connected");
    expect(ownLoginStatus(logins, "chatgpt", "agent_user", "sarah").state).toBe("not_set");
    expect(ownLoginStatus(logins, "chatgpt", "global_user").state).toBe("not_set");
    expect(ownLoginStatus(logins, "claude", "agent_user", "allan").state).toBe("not_set");
  });
});
