// Export a laid-out diagram as an `.excalidraw` scene. Pure: the layout already
// holds absolute coordinates, so this is a mapping, not a computation.
//
// Two deliberate divergences from how the renderer draws:
//   * Colours are literal hex. The renderer styles tones with CSS variables, which
//     mean nothing in a standalone file, so the export carries the same hues as
//     concrete values.
//   * Arrows are exported with explicit `points` and NULL bindings. Excalidraw's
//     binding shape has changed across versions (`focus`/`gap` → `fixedPoint`/
//     `mode`), and a binding would let the app re-route connectors we have
//     already routed. Null bindings are stable across every version and keep our
//     geometry intact; the user can re-bind in the app if they want to.
// Fields Excalidraw regenerates on import (fractional `index`) are omitted
// deliberately rather than guessed at.

import { stableSeed } from "./diagramDraw";
import type { DiagramLayout, LaidOutNode, Point } from "./diagramLayout";
import { LANE_HUES } from "./palette";
import type { DiagramTone } from "./spec";

const FONT_HAND_DRAWN = 1;
const LINE_HEIGHT = 1.25;
const FONT_SIZE = 16;
const GROUP_FONT_SIZE = 14;

const INK = "#1e1e1e";
const MUTED = "#5c5c5c";
const TONE_STROKE: Record<DiagramTone, string> = {
  default: INK,
  accent: "#2a78d6",
  good: "#1baf7a",
  warn: "#eda100",
  bad: "#e34948",
};
const TONE_FILL: Record<DiagramTone, string> = {
  default: "transparent",
  accent: "#e7f0fc",
  good: "#e3f7f0",
  warn: "#fdf3dd",
  bad: "#fbe8e8",
};

interface Element {
  [key: string]: unknown;
}

/** Every field Excalidraw expects on a persisted element, so the scene never
 *  relies on import-time defaults. */
function base(id: string, type: string, x: number, y: number, w: number, h: number): Element {
  const seed = stableSeed(id);
  return {
    id,
    type,
    x: round(x),
    y: round(y),
    width: round(w),
    height: round(h),
    angle: 0,
    strokeColor: INK,
    backgroundColor: "transparent",
    fillStyle: "solid",
    strokeWidth: 1,
    strokeStyle: "solid",
    roughness: 1,
    opacity: 100,
    groupIds: [],
    frameId: null,
    roundness: null,
    seed,
    version: 1,
    versionNonce: seed,
    isDeleted: false,
    boundElements: null,
    updated: 1,
    link: null,
    locked: false,
  };
}

const round = (n: number): number => Math.round(n * 100) / 100;

function textElement(
  id: string,
  text: string,
  x: number,
  y: number,
  w: number,
  h: number,
  options: { containerId?: string; color?: string; size?: number } = {},
): Element {
  const size = options.size ?? FONT_SIZE;
  return {
    ...base(id, "text", x, y, w, h),
    strokeColor: options.color ?? INK,
    text,
    originalText: text,
    fontSize: size,
    fontFamily: FONT_HAND_DRAWN,
    textAlign: options.containerId ? "center" : "left",
    verticalAlign: options.containerId ? "middle" : "top",
    containerId: options.containerId ?? null,
    lineHeight: LINE_HEIGHT,
    autoResize: true,
  };
}

/** A shape plus its label, wired together the way Excalidraw binds them: the
 *  text points at the container, the container lists the text. */
function labelled(
  id: string,
  type: string,
  x: number,
  y: number,
  w: number,
  h: number,
  label: string,
  style: { stroke: string; fill: string; rounded: boolean },
): Element[] {
  const textId = `${id}~label`;
  const shape: Element = {
    ...base(id, type, x, y, w, h),
    strokeColor: style.stroke,
    backgroundColor: style.fill,
    roundness: style.rounded ? { type: 3 } : null,
    boundElements: [{ id: textId, type: "text" }],
  };
  return [shape, textElement(textId, label, x, y, w, h, { containerId: id })];
}

const shapeType = (node: LaidOutNode): string => (node.shape === "diamond" ? "diamond" : "rectangle");

/** A 12%-ish tint of `hue` over white — the export's own ground, so it cannot
 *  use the renderer's `color-mix` against a theme variable. */
function tint(hue: string): string {
  const channel = (i: number) => {
    const v = parseInt(hue.slice(1 + i * 2, 3 + i * 2), 16);
    return Math.round(v * 0.12 + 255 * 0.88)
      .toString(16)
      .padStart(2, "0");
  };
  return `#${channel(0)}${channel(1)}${channel(2)}`;
}

/** Status first (reserved hues never double as identity), then the lane, then
 *  plain ink — the same precedence the stylesheet encodes. */
function paint(node: LaidOutNode): { stroke: string; fill: string } {
  if (node.tone !== "default") {
    return { stroke: TONE_STROKE[node.tone], fill: TONE_FILL[node.tone] };
  }
  const hue = node.lane ? LANE_HUES[node.lane - 1] : undefined;
  return hue ? { stroke: hue, fill: tint(hue) } : { stroke: INK, fill: "transparent" };
}

function arrowElement(
  id: string,
  points: Point[],
  label: string | undefined,
  dashed: boolean,
  arrow: "to" | "both" | "none",
): Element[] {
  const [ox, oy] = points[0];
  const xs = points.map(([x]) => x);
  const ys = points.map(([, y]) => y);
  const element: Element = {
    ...base(id, "arrow", ox, oy, Math.max(...xs) - Math.min(...xs), Math.max(...ys) - Math.min(...ys)),
    strokeColor: MUTED,
    strokeStyle: dashed ? "dashed" : "solid",
    points: points.map(([x, y]) => [round(x - ox), round(y - oy)]),
    lastCommittedPoint: null,
    startBinding: null,
    endBinding: null,
    startArrowhead: arrow === "both" ? "arrow" : null,
    endArrowhead: arrow === "none" ? null : "arrow",
    elbowed: false,
  };
  if (!label) return [element];
  const textId = `${id}~label`;
  element.boundElements = [{ id: textId, type: "text" }];
  return [
    element,
    textElement(textId, label, ox, oy, 0, 0, { containerId: id, color: MUTED, size: GROUP_FONT_SIZE }),
  ];
}

/** The laid-out diagram as an Excalidraw scene document. */
export function toExcalidraw(layout: DiagramLayout, title?: string): Record<string, unknown> {
  const elements: Element[] = [];

  for (const group of layout.groups) {
    elements.push({
      ...base(`group~${group.label}`, "rectangle", group.x, group.y, group.w, group.h),
      strokeColor: group.lane ? LANE_HUES[group.lane - 1] : MUTED,
      strokeStyle: "dashed",
      roundness: { type: 3 },
    });
    elements.push(
      textElement(
        `group~${group.label}~title`,
        group.label,
        group.x + 8,
        group.y + 4,
        group.w - 16,
        GROUP_FONT_SIZE * LINE_HEIGHT,
        { color: group.lane ? LANE_HUES[group.lane - 1] : MUTED, size: GROUP_FONT_SIZE },
      ),
    );
  }

  for (const node of layout.nodes) {
    elements.push(
      ...labelled(node.id, shapeType(node), node.x, node.y, node.w, node.h, node.label, {
        ...paint(node),
        rounded: node.shape === "round",
      }),
    );
  }

  for (const lifeline of layout.lifelines) {
    elements.push(
      ...labelled(
        lifeline.id,
        "rectangle",
        lifeline.x,
        lifeline.y,
        lifeline.w,
        lifeline.h,
        lifeline.label,
        lifeline.lane
          ? { stroke: LANE_HUES[lifeline.lane - 1], fill: tint(LANE_HUES[lifeline.lane - 1]), rounded: false }
          : { stroke: INK, fill: "transparent", rounded: false },
      ),
    );
    const x = lifeline.x + lifeline.w / 2;
    elements.push({
      ...base(`${lifeline.id}~life`, "line", x, lifeline.y + lifeline.h, 0, lifeline.bottom - lifeline.y - lifeline.h),
      strokeColor: MUTED,
      strokeStyle: "dashed",
      points: [
        [0, 0],
        [0, round(lifeline.bottom - lifeline.y - lifeline.h)],
      ],
      lastCommittedPoint: null,
      startBinding: null,
      endBinding: null,
      startArrowhead: null,
      endArrowhead: null,
    });
  }

  for (const [i, note] of layout.notes.entries()) {
    elements.push(
      ...labelled(`note~${i}`, "rectangle", note.x, note.y, note.w, note.h, note.lines.join(" "), {
        stroke: MUTED,
        fill: "transparent",
        rounded: false,
      }),
    );
  }

  for (const edge of layout.edges) {
    elements.push(
      ...arrowElement(`edge~${edge.key}`, edge.points, edge.label, edge.style === "dashed", edge.arrow),
    );
  }

  return {
    type: "excalidraw",
    version: 2,
    source: "https://github.com/driftwerk/workforce",
    elements,
    appState: { gridSize: null, viewBackgroundColor: "#ffffff", ...(title ? { name: title } : {}) },
    files: {},
  };
}
