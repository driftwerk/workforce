// Geometry → SVG path strings. Pure: the same node or edge always produces the
// same `d`, including in `sketch` style, because the jitter is drawn from a PRNG
// seeded off the element's stable key. Anything seeded off Math.random would
// shimmer on every resize, pan and re-render.

import { CYLINDER_CAP, type LaidOutNode, type Point } from "./diagramLayout";
import type { DiagramStyle } from "./spec";

const ARROW = 7;
const CORNER = 8;
const SKETCH_PASSES = 2;
const CURVE_SAMPLES = 22;

/** Two decimals: enough for sub-pixel smoothness, and it keeps the emitted path
 *  string stable enough to assert on. */
const r = (n: number): number => Math.round(n * 100) / 100;

/** FNV-1a over a stable key — the deterministic seed behind both the sketch
 *  jitter and the exported scene's element seeds. */
export function stableSeed(key: string): number {
  let h = 2166136261;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const line = (points: Point[], close: boolean): string =>
  points.map(([x, y], i) => `${i ? "L" : "M"}${r(x)} ${r(y)}`).join("") + (close ? "Z" : "");

/** One segment as a bowed quadratic with jittered ends — the pen stroke. */
function stroke(a: Point, b: Point, random: () => number): string {
  const [ax, ay] = a;
  const [bx, by] = b;
  const dx = bx - ax;
  const dy = by - ay;
  const length = Math.hypot(dx, dy) || 1;
  const amplitude = Math.min(2.4, 0.6 + length * 0.02);
  const bow = (random() * 2 - 1) * amplitude;
  const nudge = () => (random() * 2 - 1) * amplitude * 0.6;
  const cx = (ax + bx) / 2 - (dy / length) * bow;
  const cy = (ay + by) / 2 + (dx / length) * bow;
  return `M${r(ax + nudge())} ${r(ay + nudge())}Q${r(cx)} ${r(cy)} ${r(bx + nudge())} ${r(by + nudge())}`;
}

/** Every segment stroked `SKETCH_PASSES` times — the doubled line is what makes
 *  a hand-drawn outline read as hand-drawn. */
function sketch(points: Point[], close: boolean, key: string): string {
  const random = mulberry32(stableSeed(key));
  const segments: [Point, Point][] = [];
  for (let i = 0; i + 1 < points.length; i++) segments.push([points[i], points[i + 1]]);
  if (close && points.length > 2) segments.push([points[points.length - 1], points[0]]);
  let path = "";
  for (let pass = 0; pass < SKETCH_PASSES; pass++) {
    for (const [a, b] of segments) path += stroke(a, b, random);
  }
  return path;
}

function ellipsePoints(cx: number, cy: number, rx: number, ry: number): Point[] {
  return Array.from({ length: CURVE_SAMPLES }, (_, i): Point => {
    const t = (i / CURVE_SAMPLES) * Math.PI * 2;
    return [cx + rx * Math.cos(t), cy + ry * Math.sin(t)];
  });
}

/** A rounded rectangle sampled as a polygon — what the sketch skin bends. */
function roundedPoints(x: number, y: number, w: number, h: number, radius: number): Point[] {
  const rad = Math.min(radius, w / 2, h / 2);
  const arc = (cx: number, cy: number, from: number): Point[] =>
    Array.from({ length: 4 }, (_, i): Point => {
      const t = from + (i / 3) * (Math.PI / 2);
      return [cx + rad * Math.cos(t), cy + rad * Math.sin(t)];
    });
  return [
    [x + rad, y],
    [x + w - rad, y],
    ...arc(x + w - rad, y + rad, -Math.PI / 2),
    [x + w, y + h - rad],
    ...arc(x + w - rad, y + h - rad, 0),
    [x + rad, y + h],
    ...arc(x + rad, y + h - rad, Math.PI / 2),
    [x, y + rad],
    ...arc(x + rad, y + rad, Math.PI),
  ];
}

const diamondPoints = (x: number, y: number, w: number, h: number): Point[] => [
  [x + w / 2, y],
  [x + w, y + h / 2],
  [x + w / 2, y + h],
  [x, y + h / 2],
];

function cleanShape(node: LaidOutNode): string {
  const { x, y, w, h } = node;
  if (node.shape === "diamond") return line(diamondPoints(x, y, w, h), true);
  if (node.shape === "cylinder") {
    const cap = Math.min(CYLINDER_CAP, h / 4);
    const rx = w / 2;
    return (
      `M${r(x)} ${r(y + cap)}A${r(rx)} ${r(cap)} 0 0 1 ${r(x + w)} ${r(y + cap)}` +
      `A${r(rx)} ${r(cap)} 0 0 1 ${r(x)} ${r(y + cap)}` +
      `M${r(x)} ${r(y + cap)}L${r(x)} ${r(y + h - cap)}` +
      `A${r(rx)} ${r(cap)} 0 0 0 ${r(x + w)} ${r(y + h - cap)}L${r(x + w)} ${r(y + cap)}`
    );
  }
  const radius = node.shape === "round" ? h / 2 : CORNER;
  const rad = Math.min(radius, w / 2, h / 2);
  return (
    `M${r(x + rad)} ${r(y)}L${r(x + w - rad)} ${r(y)}A${r(rad)} ${r(rad)} 0 0 1 ${r(x + w)} ${r(y + rad)}` +
    `L${r(x + w)} ${r(y + h - rad)}A${r(rad)} ${r(rad)} 0 0 1 ${r(x + w - rad)} ${r(y + h)}` +
    `L${r(x + rad)} ${r(y + h)}A${r(rad)} ${r(rad)} 0 0 1 ${r(x)} ${r(y + h - rad)}` +
    `L${r(x)} ${r(y + rad)}A${r(rad)} ${r(rad)} 0 0 1 ${r(x + rad)} ${r(y)}Z`
  );
}

function sketchShape(node: LaidOutNode): string {
  const { x, y, w, h } = node;
  if (node.shape === "diamond") return sketch(diamondPoints(x, y, w, h), true, node.id);
  if (node.shape === "cylinder") {
    const cap = Math.min(CYLINDER_CAP, h / 4);
    const body: Point[] = [
      [x, y + cap],
      [x, y + h - cap],
      [x + w, y + h - cap],
      [x + w, y + cap],
    ];
    return (
      sketch(body, false, node.id) +
      sketch(ellipsePoints(x + w / 2, y + cap, w / 2, cap), true, node.id + "~cap")
    );
  }
  const radius = node.shape === "round" ? h / 2 : CORNER;
  return sketch(roundedPoints(x, y, w, h, radius), true, node.id);
}

/** The outline of a node box in the requested style. */
export const nodePath = (node: LaidOutNode, style: DiagramStyle): string =>
  style === "sketch" ? sketchShape(node) : cleanShape(node);

/** The connector polyline in the requested style. */
export const edgePath = (points: Point[], key: string, style: DiagramStyle): string =>
  style === "sketch" ? sketch(points, false, key) : line(points, false);

/** Filled arrow heads for an edge: one at the target, plus one at the source for
 *  a two-headed edge, none when the edge is undirected. */
export function arrowHeads(points: Point[], arrow: "to" | "both" | "none"): string[] {
  if (arrow === "none" || points.length < 2) return [];
  const heads = [head(points[points.length - 2], points[points.length - 1])];
  if (arrow === "both") heads.push(head(points[1], points[0]));
  return heads;
}

function head(from: Point, tip: Point): string {
  const dx = tip[0] - from[0];
  const dy = tip[1] - from[1];
  const length = Math.hypot(dx, dy) || 1;
  const ux = dx / length;
  const uy = dy / length;
  const baseX = tip[0] - ux * ARROW;
  const baseY = tip[1] - uy * ARROW;
  const wing = ARROW * 0.45;
  return line(
    [
      [tip[0], tip[1]],
      [baseX - uy * wing, baseY + ux * wing],
      [baseX + uy * wing, baseY - ux * wing],
    ],
    true,
  );
}
