import { describe, expect, it } from "vitest";

import { arrowHeads, edgePath, nodePath, stableSeed } from "./diagramDraw";
import type { LaidOutNode, Point } from "./diagramLayout";

const box: LaidOutNode = {
  id: "chk",
  label: "Stock available?",
  lines: ["Stock available?"],
  shape: "box",
  tone: "default",
  x: 20,
  y: 40,
  w: 120,
  h: 44,
};

describe("nodePath", () => {
  it("draws a clean box as a closed rounded rect", () => {
    const d = nodePath(box, "clean");
    expect(d.startsWith("M")).toBe(true);
    expect(d.endsWith("Z")).toBe(true);
    expect(d).toContain("A");
    // Stays inside its own bounds.
    for (const n of d.match(/-?\d+(\.\d+)?/g) ?? []) expect(Number(n)).toBeGreaterThanOrEqual(0);
  });

  it("draws a round shape with a full-height radius and a diamond as four corners", () => {
    expect(nodePath({ ...box, shape: "round" }, "clean")).toContain("A22 22");
    expect(nodePath({ ...box, shape: "diamond" }, "clean")).toBe("M80 40L140 62L80 84L20 62Z");
  });

  it("gives a cylinder a cap ellipse and a body", () => {
    const d = nodePath({ ...box, shape: "cylinder" }, "clean");
    expect((d.match(/M/g) ?? []).length).toBe(2);
    expect((d.match(/A/g) ?? []).length).toBe(3);
  });

  // The jitter must be identical across renders, resizes and pans, or the
  // diagram visibly shimmers — this is the guard for that.
  it("emits a byte-stable sketch path for the same node", () => {
    expect(nodePath(box, "sketch")).toBe(nodePath(box, "sketch"));
  });

  it("gives two different nodes different jitter", () => {
    expect(nodePath(box, "sketch")).not.toBe(nodePath({ ...box, id: "other" }, "sketch"));
  });

  it("strokes a sketched outline twice", () => {
    const clean = (nodePath({ ...box, shape: "diamond" }, "clean").match(/[ML]/g) ?? []).length;
    const sketched = (nodePath({ ...box, shape: "diamond" }, "sketch").match(/M/g) ?? []).length;
    expect(sketched).toBe(2 * 4);
    expect(clean).toBe(4);
  });
});

describe("edgePath and arrowHeads", () => {
  const points: Point[] = [
    [10, 10],
    [10, 50],
    [90, 50],
  ];

  it("draws a clean connector as a polyline", () => {
    expect(edgePath(points, "e1", "clean")).toBe("M10 10L10 50L90 50");
  });

  it("bends each segment of a sketched connector, stably", () => {
    const d = edgePath(points, "e1", "sketch");
    expect(d).toContain("Q");
    expect(d).toBe(edgePath(points, "e1", "sketch"));
    expect(d).not.toBe(edgePath(points, "e2", "sketch"));
  });

  it("points the head along the final segment", () => {
    const [head] = arrowHeads(points, "to");
    // Travelling +x into (90,50): the tip is the last point and the wings sit behind it.
    expect(head.startsWith("M90 50")).toBe(true);
    for (const x of [...head.matchAll(/[ML](-?\d+(?:\.\d+)?) /g)].map((m) => Number(m[1]))) {
      expect(x).toBeLessThanOrEqual(90);
    }
  });

  it("adds a second head only when the edge is two-way, and none when undirected", () => {
    expect(arrowHeads(points, "to")).toHaveLength(1);
    expect(arrowHeads(points, "both")).toHaveLength(2);
    expect(arrowHeads(points, "none")).toHaveLength(0);
    expect(arrowHeads([[0, 0]], "to")).toHaveLength(0);
  });
});

describe("stableSeed", () => {
  it("is deterministic and separates keys", () => {
    expect(stableSeed("a")).toBe(stableSeed("a"));
    expect(stableSeed("a")).not.toBe(stableSeed("b"));
    expect(Number.isInteger(stableSeed("a"))).toBe(true);
  });
});
