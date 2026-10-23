import { describe, expect, it } from "vitest";
import {
  DEFAULT_POLICY,
  canExclude,
  moveTier,
  readPolicy,
  seedOverride,
  tierRows,
  toggleTier,
} from "./login-policy";

describe("login policy", () => {
  it("seed override copies effective tiers and serves", () => {
    const effective = {
      tiers: ["global_user", "agent_shared"] as const,
      shared_serves: "owner" as const,
      overridden: false,
    };
    const seeded = seedOverride({ ...effective, tiers: [...effective.tiers] });
    expect(seeded).toEqual({ tiers: ["global_user", "agent_shared"], shared_serves: "owner" });
    expect(seeded).not.toHaveProperty("overridden");
  });

  it("last tier cannot be removed", () => {
    const policy = { tiers: ["agent_shared" as const], shared_serves: "anyone" as const };
    expect(canExclude(policy, "agent_shared")).toBe(false);
    expect(toggleTier(policy, "agent_shared")).toBe(policy);
    const two = toggleTier(policy, "system_shared");
    expect(two.tiers).toEqual(["agent_shared", "system_shared"]);
    expect(toggleTier(two, "agent_shared").tiers).toEqual(["system_shared"]);
  });

  it("move reorders", () => {
    const policy = {
      tiers: ["agent_user" as const, "global_user" as const, "system_shared" as const],
      shared_serves: "anyone" as const,
    };
    expect(moveTier(policy, "system_shared", -1).tiers).toEqual([
      "agent_user",
      "system_shared",
      "global_user",
    ]);
    expect(moveTier(policy, "agent_user", 1).tiers).toEqual([
      "global_user",
      "agent_user",
      "system_shared",
    ]);
    expect(moveTier(policy, "agent_user", -1)).toBe(policy);
    expect(moveTier(policy, "system_shared", 1)).toBe(policy);
  });

  it("reads a block with the defaults for what it leaves out", () => {
    expect(readPolicy(undefined)).toBeNull();
    expect(readPolicy(["agent_user"])).toBeNull();
    expect(readPolicy({ shared_serves: "owner" })).toEqual({ ...DEFAULT_POLICY, shared_serves: "owner" });
    expect(readPolicy({ tiers: ["global_user", "team_shared", 3, "agent_user"], shared_serves: "nobody" })).toEqual({
      tiers: ["global_user", "agent_user"],
      shared_serves: DEFAULT_POLICY.shared_serves,
    });
  });

  it("rows list the included tiers in order, then the rest", () => {
    expect(tierRows({ tiers: ["system_shared", "agent_user"], shared_serves: "anyone" })).toEqual([
      { tier: "system_shared", position: 0 },
      { tier: "agent_user", position: 1 },
      { tier: "global_user", position: null },
      { tier: "agent_shared", position: null },
    ]);
  });
});
