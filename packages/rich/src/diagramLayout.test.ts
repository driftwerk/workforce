import { describe, expect, it } from "vitest";

import {
  diagramAriaLabel,
  diagramOutline,
  layoutDiagram,
  wrapLabel,
  type DiagramLayout,
  type LaidOutNode,
  type LayoutMetrics,
  type Point,
} from "./diagramLayout";
import { validateDiagram, type DiagramSpec, type FlowSpec, type SequenceSpec } from "./spec";

const metrics: LayoutMetrics = { charWidth: 7, lineHeight: 16 };

/** Build through the validator so the tests exercise the same normalization the
 *  renderer sees — a spec literal would skip the defaults. */
function flow(payload: Record<string, unknown>): FlowSpec {
  const result = validateDiagram({ kind: "flow", ...payload });
  if (!result.ok) throw new Error(result.error);
  return result.spec as FlowSpec;
}

function sequence(payload: Record<string, unknown>): SequenceSpec {
  const result = validateDiagram({ kind: "sequence", ...payload });
  if (!result.ok) throw new Error(result.error);
  return result.spec as SequenceSpec;
}

const node = (id: string, extra: Record<string, unknown> = {}) => ({ id, label: id, ...extra });
const edge = (from: string, to: string, extra: Record<string, unknown> = {}) => ({
  from,
  to,
  ...extra,
});

const at = (layout: DiagramLayout, id: string): LaidOutNode => {
  const found = layout.nodes.find((n) => n.id === id);
  if (!found) throw new Error(`no node ${id}`);
  return found;
};

const overlaps = (a: LaidOutNode, b: LaidOutNode) =>
  a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

/** Does an axis-aligned segment cut through a box? Used to prove long edges are
 *  routed around the ranks they span rather than straight over them. */
function segmentHitsBox(a: Point, b: Point, box: LaidOutNode): boolean {
  const low = [Math.min(a[0], b[0]), Math.min(a[1], b[1])];
  const high = [Math.max(a[0], b[0]), Math.max(a[1], b[1])];
  return low[0] < box.x + box.w && box.x < high[0] && low[1] < box.y + box.h && box.y < high[1];
}

const chain = flow({
  title: "Order pipeline",
  nodes: [
    node("in", { shape: "round", label: "Order received" }),
    node("chk", { shape: "diamond", label: "Stock available?" }),
    node("ship", { label: "Pick and ship", group: "Warehouse" }),
    node("bo", { label: "Backorder", group: "Warehouse", tone: "warn" }),
  ],
  edges: [edge("in", "chk"), edge("chk", "ship", { label: "yes" }), edge("chk", "bo", { label: "no" })],
});

describe("layoutDiagram — determinism and geometry", () => {
  it("is a pure function of spec and metrics", () => {
    const a = layoutDiagram(chain, metrics);
    const b = layoutDiagram(chain, metrics);
    expect(JSON.stringify(a)).toEqual(JSON.stringify(b));
  });

  it("never overlaps two node boxes", () => {
    const layout = layoutDiagram(chain, metrics);
    for (const [i, a] of layout.nodes.entries()) {
      for (const b of layout.nodes.slice(i + 1)) {
        expect(overlaps(a, b), `${a.id} overlaps ${b.id}`).toBe(false);
      }
    }
  });

  it("advances every edge down the rank axis", () => {
    const layout = layoutDiagram(chain, metrics);
    for (const e of layout.edges) {
      expect(at(layout, e.from).y + at(layout, e.from).h).toBeLessThanOrEqual(at(layout, e.to).y);
    }
  });

  it("advances along x instead when the direction is right", () => {
    const layout = layoutDiagram({ ...chain, direction: "right" }, metrics);
    for (const e of layout.edges) {
      expect(at(layout, e.from).x + at(layout, e.from).w).toBeLessThanOrEqual(at(layout, e.to).x);
    }
    // Boxes keep their own proportions — a transposed layout would rotate the
    // labels with them.
    expect(at(layout, "in").w).toEqual(at(layoutDiagram(chain, metrics), "in").w);
  });

  it("lays out a cycle instead of hanging", () => {
    const layout = layoutDiagram(
      flow({
        nodes: [node("a"), node("b"), node("c")],
        edges: [edge("a", "b"), edge("b", "c"), edge("c", "a")],
      }),
      metrics,
    );
    expect(layout.nodes.map((n) => n.id)).toEqual(["a", "b", "c"]);
    expect(layout.edges).toHaveLength(3);
  });

  it("draws a self-edge as a loop beside its node", () => {
    const layout = layoutDiagram(
      flow({ nodes: [node("a")], edges: [edge("a", "a", { label: "retry" })] }),
      metrics,
    );
    const loop = layout.edges[0];
    expect(loop.points.length).toBeGreaterThanOrEqual(4);
    expect(Math.max(...loop.points.map(([x]) => x))).toBeGreaterThan(at(layout, "a").x + at(layout, "a").w);
  });

  it("routes a rank-spanning edge around the boxes between its ends", () => {
    const layout = layoutDiagram(
      flow({
        nodes: [node("a"), node("b"), node("c"), node("d")],
        edges: [edge("a", "b"), edge("b", "c"), edge("c", "d"), edge("a", "d")],
      }),
      metrics,
    );
    const long = layout.edges.find((e) => e.from === "a" && e.to === "d");
    expect(long).toBeDefined();
    for (const box of [at(layout, "b"), at(layout, "c")]) {
      for (let i = 0; i + 1 < long!.points.length; i++) {
        expect(segmentHitsBox(long!.points[i], long!.points[i + 1], box)).toBe(false);
      }
    }
  });
});

describe("layoutDiagram — hints, groups and dropped endpoints", () => {
  it("honours an `at` row hint that keeps edges rank-increasing", () => {
    const spec = flow({
      nodes: [node("a"), node("b"), node("c", { at: [0, 3] })],
      edges: [edge("a", "b")],
    });
    const hinted = layoutDiagram(spec, metrics);
    expect(at(hinted, "c").y).toBeGreaterThan(at(hinted, "b").y);
  });

  it("ignores an `at` hint that would invert an edge", () => {
    const spec = flow({
      nodes: [node("a"), node("b", { at: [0, 0] })],
      edges: [edge("a", "b")],
    });
    const layout = layoutDiagram(spec, metrics);
    expect(at(layout, "a").y + at(layout, "a").h).toBeLessThanOrEqual(at(layout, "b").y);
  });

  it("drops an edge whose endpoint names no node and reports it", () => {
    const layout = layoutDiagram(
      flow({
        nodes: [node("a"), node("b")],
        edges: [edge("a", "b"), edge("a", "ghost")],
      }),
      metrics,
    );
    expect(layout.edges).toHaveLength(1);
    expect(layout.dropped).toEqual(["a → ghost"]);
  });

  it("boxes each group without overlapping another", () => {
    const layout = layoutDiagram(
      flow({
        nodes: [
          node("a", { group: "Left" }),
          node("b", { group: "Left" }),
          node("c", { group: "Right" }),
          node("d", { group: "Right" }),
        ],
        edges: [edge("a", "c"), edge("b", "d")],
      }),
      metrics,
    );
    expect(layout.groups.map((g) => g.label)).toEqual(["Left", "Right"]);
    for (const [i, a] of layout.groups.entries()) {
      for (const b of layout.groups.slice(i + 1)) {
        const hit = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
        expect(hit).toBe(false);
      }
    }
  });

  it("keeps the whole diagram inside its reported extent", () => {
    const layout = layoutDiagram(chain, metrics);
    for (const n of layout.nodes) {
      expect(n.x).toBeGreaterThanOrEqual(0);
      expect(n.y).toBeGreaterThanOrEqual(0);
      expect(n.x + n.w).toBeLessThanOrEqual(layout.width);
      expect(n.y + n.h).toBeLessThanOrEqual(layout.height);
    }
  });
});

describe("layoutDiagram — sequence", () => {
  const refund = sequence({
    title: "Refund request",
    participants: [
      { id: "c", label: "Customer" },
      { id: "a", label: "Agent" },
      { id: "p", label: "Payments" },
    ],
    steps: [
      { from: "c", to: "a", label: "Requests refund" },
      { from: "a", to: "p", label: "Check eligibility" },
      { from: "p", to: "a", label: "Eligible", style: "dashed" },
    ],
  });

  it("places participants as columns in declared order", () => {
    const layout = layoutDiagram(refund, metrics);
    expect(layout.lifelines.map((l) => l.id)).toEqual(["c", "a", "p"]);
    const xs = layout.lifelines.map((l) => l.x);
    expect([...xs].sort((a, b) => a - b)).toEqual(xs);
    for (const l of layout.lifelines) expect(l.bottom).toBeGreaterThan(l.y + l.h);
  });

  it("keeps the dashed reply dashed and steps in time order", () => {
    const layout = layoutDiagram(refund, metrics);
    expect(layout.edges.map((e) => e.style)).toEqual(["solid", "solid", "dashed"]);
    const ys = layout.edges.map((e) => e.points[0][1]);
    expect([...ys].sort((a, b) => a - b)).toEqual(ys);
  });

  it("gives a self-step more room than a plain step", () => {
    const plain = layoutDiagram(
      sequence({
        participants: [
          { id: "a", label: "A" },
          { id: "b", label: "B" },
        ],
        steps: [{ from: "a", to: "b" }, { from: "a", to: "b" }],
      }),
      metrics,
    );
    const looped = layoutDiagram(
      sequence({
        participants: [
          { id: "a", label: "A" },
          { id: "b", label: "B" },
        ],
        steps: [{ from: "a", to: "b" }, { from: "a", to: "a" }],
      }),
      metrics,
    );
    // A self-loop's first point is its top, so compare baselines (the lowest
    // point each step reaches) to measure the row it actually consumes.
    const baseline = (l: DiagramLayout, i: number) => Math.max(...l.edges[i].points.map(([, y]) => y));
    expect(baseline(looped, 1) - baseline(looped, 0)).toBeGreaterThan(
      baseline(plain, 1) - baseline(plain, 0),
    );
  });

  it("reserves the note's real height so the next step clears it", () => {
    const layout = layoutDiagram(
      sequence({
        participants: [
          { id: "a", label: "A" },
          { id: "b", label: "B" },
        ],
        steps: [
          { from: "a", to: "b", note: "A long aside that wraps onto more than a single line of text" },
          { from: "b", to: "a" },
        ],
      }),
      metrics,
    );
    const note = layout.notes[0];
    expect(note).toBeDefined();
    expect(layout.edges[1].points[0][1]).toBeGreaterThan(note.y + note.h);
  });

  it("drops a step naming an unknown participant", () => {
    const layout = layoutDiagram(
      sequence({
        participants: [
          { id: "a", label: "A" },
          { id: "b", label: "B" },
        ],
        steps: [{ from: "a", to: "b" }, { from: "a", to: "z" }],
      }),
      metrics,
    );
    expect(layout.edges).toHaveLength(1);
    expect(layout.dropped).toEqual(["a → z"]);
  });
});

describe("labels and accessible text", () => {
  it("wraps on words and hard-breaks anything longer than the line", () => {
    expect(wrapLabel("one two three", 9)).toEqual(["one two", "three"]);
    expect(wrapLabel("supercalifragilistic", 8)).toEqual(["supercal", "ifragili", "stic"]);
  });

  it("names the diagram and its size in the aria label", () => {
    const layout = layoutDiagram(chain, metrics);
    expect(diagramAriaLabel(chain, layout)).toBe("Flow diagram: Order pipeline; 4 steps");
  });

  it("walks the graph as sentences for screen readers", () => {
    const layout = layoutDiagram(chain, metrics);
    expect(diagramOutline(layout)).toEqual([
      "Order received → Stock available?",
      "Stock available? → Pick and ship (yes)",
      "Stock available? → Backorder (no)",
    ]);
  });

  it("names participants, not ids, in a sequence outline", () => {
    const spec: DiagramSpec = sequence({
      participants: [
        { id: "c", label: "Customer" },
        { id: "a", label: "Agent" },
      ],
      steps: [{ from: "c", to: "a", label: "Asks" }],
    });
    expect(diagramOutline(layoutDiagram(spec, metrics))).toEqual(["Customer → Agent (Asks)"]);
  });
});
