import { describe, expect, it } from "vitest";

// `?raw` gives the stylesheet as a string through vite, so this needs no node
// types and no filesystem access.
import css from "./styles.css?raw";

import { layoutDiagram, MAX_LANES, type LayoutMetrics } from "./diagramLayout";
import { toExcalidraw } from "./excalidraw";
import { LANE_HUES } from "./palette";
import { validateDiagram, type DiagramSpec } from "./spec";

const metrics: LayoutMetrics = { charWidth: 7, lineHeight: 16 };

const build = (payload: Record<string, unknown>): DiagramSpec => {
  const result = validateDiagram(payload);
  if (!result.ok) throw new Error(result.error);
  return result.spec;
};

const grouped = (groups: (string | undefined)[]) =>
  build({
    kind: "flow",
    nodes: groups.map((g, i) => ({ id: `n${i}`, label: `Step ${i}`, ...(g ? { group: g } : {}) })),
    edges: groups.slice(1).map((_, i) => ({ from: `n${i}`, to: `n${i + 1}` })),
  });

const lanesOf = (spec: DiagramSpec) => layoutDiagram(spec, metrics).nodes.map((n) => n.lane);

describe("lane assignment", () => {
  it("gives each group a slot in first-appearance order", () => {
    expect(lanesOf(grouped(["Warehouse", "Warehouse", "Finance", "Warehouse"]))).toEqual([1, 1, 2, 1]);
  });

  it("leaves ungrouped nodes without a lane", () => {
    expect(lanesOf(grouped([undefined, "Ops", undefined]))).toEqual([undefined, 1, undefined]);
  });

  it("colours nothing when no node is grouped", () => {
    const layout = layoutDiagram(grouped([undefined, undefined]), metrics);
    expect(layout.nodes.every((n) => n.lane === undefined)).toBe(true);
  });

  // Cycling would claim two different groups are the same group.
  it("drops past the cap to neutral rather than reusing a hue", () => {
    const many = ["a", "b", "c", "d", "e", "f", "g"];
    const lanes = lanesOf(grouped(many));
    expect(lanes.slice(0, MAX_LANES)).toEqual([1, 2, 3, 4, 5]);
    expect(lanes.slice(MAX_LANES)).toEqual([undefined, undefined]);
  });

  it("keeps a group's slot stable when a step is inserted before it", () => {
    const before = lanesOf(grouped(["Ops", "Finance"]));
    const after = lanesOf(grouped(["Ops", "Ops", "Finance"]));
    expect(before).toEqual([1, 2]);
    expect(after).toEqual([1, 1, 2]);
  });

  it("gives group boxes their members' lane", () => {
    const layout = layoutDiagram(grouped(["Warehouse", "Finance"]), metrics);
    expect(layout.groups.map((g) => [g.label, g.lane])).toEqual([
      ["Warehouse", 1],
      ["Finance", 2],
    ]);
  });

  it("lanes a sequence by participant", () => {
    const layout = layoutDiagram(
      build({
        kind: "sequence",
        participants: [
          { id: "c", label: "Customer" },
          { id: "a", label: "Agent" },
        ],
        steps: [{ from: "c", to: "a" }],
      }),
      metrics,
    );
    expect(layout.lifelines.map((l) => l.lane)).toEqual([1, 2]);
  });
});

describe("lane colour reaches the export", () => {
  const layout = layoutDiagram(
    build({
      kind: "flow",
      nodes: [
        { id: "a", label: "Pick", group: "Warehouse" },
        { id: "b", label: "Invoice", group: "Finance" },
        { id: "c", label: "Failed", group: "Finance", tone: "bad" },
        { id: "d", label: "Plain" },
      ],
      edges: [{ from: "a", to: "b" }],
    }),
    metrics,
  );
  const byId = new Map(
    (toExcalidraw(layout).elements as Record<string, unknown>[]).map((e) => [e.id as string, e]),
  );

  it("paints each node with its lane hue", () => {
    expect(byId.get("a")!.strokeColor).toBe(LANE_HUES[0]);
    expect(byId.get("b")!.strokeColor).toBe(LANE_HUES[1]);
  });

  // Status hues are reserved and must never double as identity.
  it("lets a status tone outrank the lane", () => {
    expect(byId.get("c")!.strokeColor).toBe("#e34948");
    expect(byId.get("c")!.backgroundColor).toBe("#fbe8e8");
  });

  it("leaves an ungrouped node in plain ink", () => {
    expect(byId.get("d")!.strokeColor).toBe("#1e1e1e");
    expect(byId.get("d")!.backgroundColor).toBe("transparent");
  });

  it("tints the fill rather than flooding it", () => {
    const fill = byId.get("a")!.backgroundColor as string;
    expect(fill).toMatch(/^#[0-9a-f]{6}$/);
    expect(fill).not.toBe(LANE_HUES[0]);
  });
});

// The renderer paints from tokens and the export from LANE_HUES; nothing
// but this test stops the two drifting into different palettes.
describe("stylesheet and palette agree", () => {
  it("declares the same light hues the export uses", () => {
    for (const [i, hue] of LANE_HUES.entries()) {
      expect(css, `--rich-lane-${i + 1} must be ${hue}`).toContain(`--rich-lane-${i + 1}: ${hue};`);
    }
  });

  it("declares a distinct, complete dark set", () => {
    const dark = css.slice(css.indexOf(':where(:root[data-theme="dark"])'));
    const hues = [...dark.matchAll(/--rich-lane-(\d): (#[0-9a-f]{6});/g)].slice(0, LANE_HUES.length);
    expect(hues).toHaveLength(LANE_HUES.length);
    // Selected for the dark band, not derived — most steps must differ.
    const changed = hues.filter(([, slot, hue]) => hue !== LANE_HUES[Number(slot) - 1]);
    expect(changed.length).toBeGreaterThanOrEqual(3);
  });

  it("has a rule for every lane slot and no more", () => {
    for (let i = 1; i <= LANE_HUES.length; i++) {
      expect(css).toContain(`.wf-dg-node.lane-${i} .wf-dg-shape`);
    }
    expect(css).not.toContain(`.wf-dg-node.lane-${LANE_HUES.length + 1} `);
  });

  // Equal specificity, so source order is what makes status win.
  it("orders tone rules after lane rules so status wins", () => {
    expect(css.indexOf(".wf-dg-node.tone-bad")).toBeGreaterThan(css.indexOf(".wf-dg-node.lane-5"));
  });
});
