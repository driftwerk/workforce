import { describe, expect, it } from "vitest";
import { ApiError } from "@/lib/api-client";
import type { DeviceFlowView } from "@/lib/api/subscriptions";
import {
  afterConnect,
  connectView,
  flowTargetLabel,
  nextFlowUi,
  pollFlow,
  secondsLeft,
  watchedFlow,
  type PendingFlow,
} from "./device-flow";

const pending: PendingFlow = {
  state: "pending",
  flow_id: "f1",
  user_code: "ABCD-1234",
  verification_url: "https://auth.openai.com/codex/device",
  expires_at: "2026-10-01T10:15:00Z",
  interval_secs: 5,
};

describe("device flow", () => {
  it("404 poll maps to expired", async () => {
    const gone = () => Promise.reject(new ApiError(404, '{"error":"no such sign-in"}'));
    await expect(pollFlow(gone, "f1")).resolves.toMatchObject({ state: "expired", flow_id: "f1" });
    const broken = () => Promise.reject(new ApiError(500, "boom"));
    await expect(pollFlow(broken, "f1")).rejects.toThrow("boom");
  });

  it("unsupported opens paste", () => {
    const unsupported: DeviceFlowView = { state: "unsupported", flow_id: "f1", reason: "paste instead" };
    expect(nextFlowUi(unsupported, false)).toMatchObject({ showPaste: true, failure: "paste instead", canStart: true });
    expect(nextFlowUi(pending, false).showPaste).toBe(false);
  });

  it("terminal state stops polling", () => {
    expect(nextFlowUi(pending, false)).toMatchObject({ pollMs: 5000, canStart: false });
    const done: DeviceFlowView = { state: "completed", flow_id: "f1", account: { email: "a@x", plan: "plus" } };
    expect(nextFlowUi(done, false)).toMatchObject({ pollMs: false, connected: { email: "a@x" } });
    for (const state of ["expired", "failed", "cancelled"] as const) {
      expect(nextFlowUi({ state, flow_id: "f1", reason: "r" }, false).pollMs).toBe(false);
    }
    expect(nextFlowUi(null, true).canStart).toBe(false);
  });

  it("countdown derives from server expires_at", () => {
    expect(secondsLeft(pending, new Date("2026-10-01T10:00:00Z"))).toBe(900);
    expect(secondsLeft(pending, new Date("2026-10-01T10:14:30.400Z"))).toBe(29);
    expect(secondsLeft(pending, new Date("2026-10-01T10:20:00Z"))).toBe(0);
  });

  it("a sign-in waiting for another login shows only on the global card, and a new start warns it replaces it", () => {
    const forAllan: DeviceFlowView = {
      ...pending,
      target: { tier: "agent_user", agent_id: "allan", agent_name: "Allan" },
    };
    expect(connectView(forAllan, { tier: "global_user" }, false)).toEqual({
      shown: forAllan,
      replaces: forAllan.target,
      canStart: true,
    });
    expect(connectView(forAllan, { tier: "agent_shared", agent_id: "allan" }, false)).toEqual({
      shown: null,
      replaces: forAllan.target,
      canStart: true,
    });
    expect(connectView(forAllan, { tier: "agent_user", agent_id: "allan" }, false)).toEqual({
      shown: forAllan,
      replaces: null,
      canStart: false,
    });
    const settled: DeviceFlowView = { ...forAllan, state: "expired", reason: "expired" } as DeviceFlowView;
    expect(connectView(settled, { tier: "global_user" }, false)).toEqual({
      shown: null,
      replaces: null,
      canStart: true,
    });
    expect(connectView(null, { tier: "global_user" }, true).canStart).toBe(false);
  });

  it("names what a sign-in is for", () => {
    expect(flowTargetLabel({ tier: "global_user", agent_name: null })).toBe("for every agent");
    expect(flowTargetLabel({ tier: "agent_user", agent_id: "allan", agent_name: "Allan" })).toBe("for Allan");
    expect(flowTargetLabel({ tier: "agent_shared", agent_id: "allan", agent_name: null })).toBe(
      "for allan's shared login",
    );
  });

  it("the kept sign-in shows, refreshed by its own poll", () => {
    const forAllan: DeviceFlowView = {
      ...pending,
      flow_id: "f2",
      target: { tier: "agent_user", agent_id: "allan", agent_name: "Allan" },
    };
    expect(watchedFlow(forAllan, undefined, "f1")).toBe(forAllan);
    const done: DeviceFlowView = { state: "completed", flow_id: "f2", account: { email: "a@x", plan: null } };
    expect(watchedFlow(forAllan, done, "f2")).toEqual({ ...done, target: forAllan.target });
    const older: DeviceFlowView = { state: "cancelled", flow_id: "f1", reason: "replaced" };
    expect(watchedFlow(forAllan, older, "f1")).toBe(forAllan);
  });

  it("a settled sign-in shows only on the card that started it or saw it waiting", () => {
    const expired: DeviceFlowView = { state: "expired", flow_id: "f1", reason: "The code expired" };
    expect(watchedFlow(expired, undefined, null)).toBeNull();
    expect(watchedFlow(expired, undefined, "f0")).toBeNull();
    expect(watchedFlow(expired, undefined, "f1")).toBe(expired);
    expect(watchedFlow(null, undefined, "f1")).toBeNull();
  });
});

describe("afterConnect", () => {
  it("promises a re-run only for a prompt whose message waits on the sign-in", () => {
    const prompt = { kind: "chatgpt" as const, agent_id: "allan", reason: "r", private: true };
    expect(afterConnect({ ...prompt, resumes: true })).toBe("Re-running your message…");
    expect(afterConnect({ ...prompt, resumes: false })).toBeUndefined();
  });
});
