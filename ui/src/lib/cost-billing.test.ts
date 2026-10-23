import { describe, expect, it } from "vitest";
import { billedCost, billingLabel, originLabel } from "./cost-billing";

describe("billingLabel", () => {
  it("shows the plan a subscription row names", () => {
    expect(billingLabel({ billing: "subscription", plan: "Alice's plan (this agent)" })).toBe(
      "Subscription · Alice's plan (this agent)",
    );
    expect(billingLabel({ billing: "subscription", plan: "Install login" })).toBe("Subscription · Install login");
  });

  it("reads a metered row as metered", () => {
    expect(billingLabel({ billing: "metered", plan: null })).toBe("Metered");
  });
});

describe("billedCost", () => {
  it("marks subscription amounts as estimates", () => {
    expect(billedCost(0.12, "subscription")).toBe("~$0.12");
    expect(billedCost(0.12, "metered")).toBe("$0.12");
  });
});

describe("originLabel", () => {
  it("reads rows from before origins were kept as unrecorded", () => {
    expect(originLabel(null)).toBe("unrecorded (before 2026-09-29)");
    expect(originLabel("self_learning")).toBe("self learning");
  });
});
