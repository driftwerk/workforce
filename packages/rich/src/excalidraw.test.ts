import { describe, expect, it } from "vitest";

import { layoutDiagram, type LayoutMetrics } from "./diagramLayout";
import { toExcalidraw } from "./excalidraw";
import { validateDiagram, type DiagramSpec } from "./spec";

const metrics: LayoutMetrics = { charWidth: 7, lineHeight: 16 };

const build = (payload: Record<string, unknown>): DiagramSpec => {
  const result = validateDiagram(payload);
  if (!result.ok) throw new Error(result.error);
  return result.spec;
};

const spec = build({
  kind: "flow",
  title: "Order pipeline",
  nodes: [
    { id: "in", label: "Order received", shape: "round" },
    { id: "chk", label: "Stock available?", shape: "diamond" },
    { id: "ship", label: "Pick and ship", group: "Warehouse" },
    { id: "bo", label: "Backorder", group: "Warehouse", tone: "warn" },
  ],
  edges: [
    { from: "in", to: "chk" },
    { from: "chk", to: "ship", label: "yes" },
    { from: "chk", to: "bo", label: "no", style: "dashed" },
  ],
});

const scene = toExcalidraw(layoutDiagram(spec, metrics), spec.title);
const elements = scene.elements as Record<string, unknown>[];
const byId = new Map(elements.map((e) => [e.id as string, e] as const));

// The fields Excalidraw expects on every persisted element. Pinned so a partial
// element (which the app would silently drop or repair) fails here first.
const BASE_FIELDS = [
  "id",
  "type",
  "x",
  "y",
  "width",
  "height",
  "angle",
  "strokeColor",
  "backgroundColor",
  "fillStyle",
  "strokeWidth",
  "strokeStyle",
  "roughness",
  "opacity",
  "groupIds",
  "frameId",
  "roundness",
  "seed",
  "version",
  "versionNonce",
  "isDeleted",
  "boundElements",
  "updated",
  "link",
  "locked",
];

describe("toExcalidraw", () => {
  it("produces a well-formed scene document", () => {
    expect(scene.type).toBe("excalidraw");
    expect(scene.version).toBe(2);
    expect(scene.files).toEqual({});
    expect((scene.appState as Record<string, unknown>).name).toBe("Order pipeline");
  });

  it("gives every element the full base field set and a unique id", () => {
    for (const element of elements) {
      for (const field of BASE_FIELDS) expect(element, `${element.id} lacks ${field}`).toHaveProperty(field);
      expect(Number.isInteger(element.seed)).toBe(true);
    }
    expect(new Set(elements.map((e) => e.id)).size).toBe(elements.length);
  });

  it("maps shapes and tones onto excalidraw types and literal colours", () => {
    expect(byId.get("chk")!.type).toBe("diamond");
    expect(byId.get("in")!.type).toBe("rectangle");
    expect(byId.get("in")!.roundness).toEqual({ type: 3 });
    expect(byId.get("ship")!.roundness).toBeNull();
    // Tones and lanes are CSS variables in the renderer; a standalone file needs
    // real hex. `bo` is toned, so status outranks its Warehouse lane; `ship`
    // takes the lane; `chk` has no group and stays ink.
    expect(byId.get("bo")!.strokeColor).toBe("#eda100");
    expect(byId.get("ship")!.strokeColor).toBe("#2a78d6");
    expect(byId.get("chk")!.strokeColor).toBe("#1e1e1e");
  });

  it("binds each label to its container both ways", () => {
    const shape = byId.get("chk")!;
    const text = byId.get("chk~label")!;
    expect(shape.boundElements).toEqual([{ id: "chk~label", type: "text" }]);
    expect(text.type).toBe("text");
    expect(text.containerId).toBe("chk");
    expect(text.text).toBe("Stock available?");
    expect(text.originalText).toBe("Stock available?");
  });

  it("exports arrows with explicit relative points and null bindings", () => {
    const arrows = elements.filter((e) => e.type === "arrow");
    expect(arrows).toHaveLength(3);
    for (const arrow of arrows) {
      // Bindings would let Excalidraw re-route connectors we already routed, and
      // their schema has churned across versions.
      expect(arrow.startBinding).toBeNull();
      expect(arrow.endBinding).toBeNull();
      expect(arrow.endArrowhead).toBe("arrow");
      const points = arrow.points as [number, number][];
      expect(points.length).toBeGreaterThanOrEqual(2);
      expect(points[0]).toEqual([0, 0]);
    }
    expect(arrows.find((a) => (a.boundElements as { id: string }[] | null)?.length)).toBeDefined();
    expect(arrows.some((a) => a.strokeStyle === "dashed")).toBe(true);
  });

  it("draws a group as a dashed box with its own caption", () => {
    expect(byId.get("group~Warehouse")!.strokeStyle).toBe("dashed");
    expect(byId.get("group~Warehouse~title")!.text).toBe("Warehouse");
    expect(byId.get("group~Warehouse~title")!.containerId).toBeNull();
  });

  it("exports a sequence as heads, dashed lifelines and messages", () => {
    const sequence = build({
      kind: "sequence",
      participants: [
        { id: "c", label: "Customer" },
        { id: "a", label: "Agent" },
      ],
      steps: [{ from: "c", to: "a", label: "Asks", note: "Logged in the CRM" }],
    });
    const out = toExcalidraw(layoutDiagram(sequence, metrics)).elements as Record<string, unknown>[];
    const kinds = out.map((e) => e.type);
    expect(kinds.filter((k) => k === "line")).toHaveLength(2);
    expect(kinds.filter((k) => k === "arrow")).toHaveLength(1);
    expect(out.find((e) => e.id === "c~label")!.text).toBe("Customer");
    expect(out.find((e) => e.id === "note~0~label")!.text).toBe("Logged in the CRM");
  });
});
