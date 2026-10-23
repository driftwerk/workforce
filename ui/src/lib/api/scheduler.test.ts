import { describe, expect, it } from "vitest";
import { updateRefusal } from "./scheduler";

describe("updateRefusal", () => {
  it("reads the cost to approve off a needs_confirmation answer", () => {
    const body = {
      status: "needs_confirmation",
      reason: "projected $34.56/day exceeds the $5.00/day confirmation threshold",
      estimate: { per_run_usd: 0.12, runs_per_day: 288, per_day_usd: 34.56, per_month_usd: 1036.8 },
    };
    expect(updateRefusal(new Error(JSON.stringify(body)))).toEqual({ confirmation: body });
  });

  it("reads the reason off a refused edit", () => {
    const refused = new Error(JSON.stringify({ error: "a reminder is always private" }));
    expect(updateRefusal(refused)).toEqual({ message: "a reminder is always private" });
  });

  it("keeps a body that is not JSON as the message", () => {
    expect(updateRefusal(new Error("Forbidden"))).toEqual({ message: "Forbidden" });
  });
});
