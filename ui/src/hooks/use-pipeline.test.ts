import { describe, expect, it } from "vitest";
import { sameBoard } from "./use-pipeline";

const board = (scope: string, id: string, windows = "") => ({ queryKey: ["pipeline-items", scope, id, windows] });

describe("sameBoard", () => {
  const previous = { pipeline: "orders" };
  const keep = sameBoard("agent:example", "orders");

  it("keeps the board on screen while the same pipeline's windows or replay load", () => {
    expect(keep(previous, board("agent:example", "orders"))).toBe(previous);
    expect(keep(previous, board("agent:example", "orders", "verified:30d"))).toBe(previous);
  });

  it("never shows another pipeline's board", () => {
    expect(keep(previous, board("agent:example", "triage"))).toBeUndefined();
    expect(keep(previous, board("user:kings", "orders"))).toBeUndefined();
    expect(keep(previous, undefined)).toBeUndefined();
  });
});
