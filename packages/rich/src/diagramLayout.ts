// Turn a validated diagram spec into absolute geometry. Pure and DOM-free: text
// metrics arrive as numbers, so the same input always yields byte-identical
// output — the sketch skin seeds its jitter off this, and a diagram that
// reshuffled on every resize would be unusable.
//
// `flow` is a layered (Sugiyama-style) layout: rank, order to reduce crossings,
// assign coordinates, route orthogonally. `sequence` needs no engine at all —
// participants are columns and steps are rows.
//
// Everything is computed in an abstract (main, cross) frame: `main` is the axis
// ranks advance along, `cross` the one siblings spread across. `direction`
// picks which is x and which is y, so there is one code path and node boxes are
// never transposed (a rotated box would rotate its label with it).

import type {
  DiagramEdge,
  DiagramShape,
  DiagramSpec,
  DiagramTone,
  EdgeArrow,
  EdgeStyle,
  FlowSpec,
  SequenceSpec,
} from "./spec";

export interface LayoutMetrics {
  /** Mean advance of the block font at label size. */
  charWidth: number;
  lineHeight: number;
}

export type Point = [number, number];

export interface LaidOutNode {
  id: string;
  label: string;
  lines: string[];
  shape: DiagramShape;
  tone: DiagramTone;
  x: number;
  y: number;
  w: number;
  h: number;
  group?: string;
  /** 1-based lane slot for the node's group, or absent when it has no group or
   *  its group is past {@link MAX_LANES}. Identity only — a node's own `tone`
   *  outranks it, since status colours are reserved. */
  lane?: number;
}

export interface LaidOutEdge {
  key: string;
  from: string;
  to: string;
  points: Point[];
  label?: string;
  labelAt?: Point;
  style: EdgeStyle;
  arrow: EdgeArrow;
}

export interface LaidOutGroup {
  label: string;
  lane?: number;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface LaidOutLifeline {
  id: string;
  lane?: number;
  label: string;
  lines: string[];
  x: number;
  y: number;
  w: number;
  h: number;
  bottom: number;
}

export interface LaidOutNote {
  lines: string[];
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface DiagramLayout {
  kind: "flow" | "sequence";
  nodes: LaidOutNode[];
  edges: LaidOutEdge[];
  groups: LaidOutGroup[];
  lifelines: LaidOutLifeline[];
  notes: LaidOutNote[];
  width: number;
  height: number;
  /** Endpoints that named no declared node, reported rather than swallowed. */
  dropped: string[];
}

const PAD = 14;
const NODE_PAD_X = 12;
const NODE_PAD_Y = 8;
const MIN_NODE_W = 76;
const WRAP_CHARS = 22;
const NODE_GAP = 22;
const RANK_GAP = 46;
const DIAMOND_SCALE = 1.4;
/** Height of a cylinder's elliptical cap — layout reserves it, drawing uses it. */
export const CYLINDER_CAP = 6;
const GROUP_PAD = 14;
const GROUP_LABEL_H = 18;
const DUMMY_SIZE = 10;
const LOOP_OFFSET = 24;
const LOOP_STEP = 10;
const SELF_LOOP = 22;
const ORDER_SWEEPS = 4;
const TRANSPOSE_ROUNDS = 4;
const REFINE_PASSES = 2;

/** How many groups get their own hue. Beyond this they go neutral rather than
 *  cycling — a repeated hue would claim two different groups are the same one. */
export const MAX_LANES = 5;

/** Lane slots by first appearance, so adding a step never repaints the lanes
 *  around it. */
function assignLanes(names: (string | undefined)[]): Map<string, number> {
  const lanes = new Map<string, number>();
  for (const name of names) {
    if (!name || lanes.has(name)) continue;
    if (lanes.size >= MAX_LANES) continue;
    lanes.set(name, lanes.size + 1);
  }
  return lanes;
}

const MIN_COL_W = 92;
const COL_GAP = 34;
const HEAD_PAD_X = 10;
const HEAD_PAD_Y = 7;
const STEP_GAP = 44;
const SELF_STEP_GAP = 66;
const NOTE_GAP = 34;
const NOTE_PAD = 8;
const LIFELINE_TAIL = 18;

/** Greedy word wrap, hard-breaking any word longer than the line. */
export function wrapLabel(text: string, maxChars: number): string[] {
  const lines: string[] = [];
  let line = "";
  for (const word of text.split(/\s+/).filter(Boolean)) {
    if (line && line.length + 1 + word.length <= maxChars) {
      line += " " + word;
    } else {
      if (line) lines.push(line);
      line = word;
    }
    while (line.length > maxChars) {
      lines.push(line.slice(0, maxChars));
      line = line.slice(maxChars);
    }
  }
  if (line) lines.push(line);
  return lines.length ? lines : [text];
}

function textBox(lines: string[], m: LayoutMetrics, padX: number, padY: number, minW: number) {
  const longest = lines.reduce((a, l) => Math.max(a, l.length), 0);
  return {
    w: Math.max(minW, Math.round(longest * m.charWidth + 2 * padX)),
    h: Math.round(lines.length * m.lineHeight + 2 * padY),
  };
}

function nodeBox(lines: string[], shape: DiagramShape, m: LayoutMetrics) {
  const { w, h } = textBox(lines, m, NODE_PAD_X, NODE_PAD_Y, MIN_NODE_W);
  if (shape === "diamond") {
    return { w: Math.round(w * DIAMOND_SCALE), h: Math.round(h * DIAMOND_SCALE) };
  }
  if (shape === "cylinder") return { w, h: h + 2 * CYLINDER_CAP };
  return { w, h };
}

const median = (sorted: number[]): number => {
  const mid = sorted.length >> 1;
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
};

/** One laid-out box in the abstract frame — a real node or an edge's waypoint. */
interface Cell {
  key: string;
  rank: number;
  main: number;
  cross: number;
  mainSize: number;
  crossSize: number;
  group?: string;
  node?: LaidOutNode;
}

/** An edge reduced to the cell chain it threads, plus how it must be drawn. */
interface Routed {
  edge: DiagramEdge;
  key: string;
  chain: string[];
  mode: "forward" | "back" | "self";
}

function flowLayout(spec: FlowSpec, m: LayoutMetrics): DiagramLayout {
  const ids = spec.nodes.map((n) => n.id);
  const known = new Set(ids);
  const dropped: string[] = [];
  const edges: DiagramEdge[] = [];
  for (const e of spec.edges) {
    // Resolution lives here, not in validation: an endpoint that names no node
    // is dropped and reported, never silently swallowed.
    if (!known.has(e.from) || !known.has(e.to)) {
      dropped.push(`${e.from} → ${e.to}`);
      continue;
    }
    edges.push(e);
  }

  const lanes = assignLanes(spec.nodes.map((n) => n.group));
  const cells = new Map<string, Cell>();
  for (const n of spec.nodes) {
    const lines = wrapLabel(n.label, WRAP_CHARS);
    const { w, h } = nodeBox(lines, n.shape, m);
    const vertical = spec.direction === "down";
    cells.set(n.id, {
      key: n.id,
      rank: 0,
      main: 0,
      cross: 0,
      mainSize: vertical ? h : w,
      crossSize: vertical ? w : h,
      group: n.group,
      node: {
        id: n.id,
        label: n.label,
        lines,
        shape: n.shape,
        tone: n.tone,
        x: 0,
        y: 0,
        w,
        h,
        group: n.group,
        lane: n.group ? lanes.get(n.group) : undefined,
      },
    });
  }

  const linking = edges.filter((e) => e.from !== e.to);
  const back = backEdges(ids, linking);
  const forward = linking.filter((e) => !back.has(e));

  const rank = rankNodes(ids, forward);
  applyHints(spec, rank, forward);
  compactRanks(rank);
  for (const [id, r] of rank) cells.get(id)!.rank = r;

  const routed: Routed[] = [];
  for (const [i, e] of edges.entries()) {
    const key = `${e.from}|${e.to}|${i}`;
    if (e.from === e.to) {
      routed.push({ edge: e, key, chain: [e.from], mode: "self" });
    } else if (back.has(e)) {
      routed.push({ edge: e, key, chain: [e.from, e.to], mode: "back" });
    } else {
      const span = rank.get(e.to)! - rank.get(e.from)!;
      const chain = [e.from];
      for (let step = 1; step < span; step++) {
        const dummy = `~${key}~${step}`;
        cells.set(dummy, {
          key: dummy,
          rank: rank.get(e.from)! + step,
          main: 0,
          cross: 0,
          mainSize: DUMMY_SIZE,
          crossSize: DUMMY_SIZE,
        });
        chain.push(dummy);
      }
      chain.push(e.to);
      routed.push({ edge: e, key, chain, mode: "forward" });
    }
  }
  const chains = routed.filter((r) => r.mode === "forward").map((r) => r.chain);
  const ranks = orderRanks(cells, chains);
  assignCross(ranks, chains, cells);
  assignMain(ranks);

  const geometry = placeCells(cells, spec.direction);
  const nodes = spec.nodes.map((n) => cells.get(n.id)!.node!);
  const groups = groupBoxes(nodes);
  const laidOutEdges = routed.map((r, i) => routeEdge(r, cells, spec.direction, geometry.crossMax, i));

  return normalize({
    kind: "flow",
    nodes,
    edges: laidOutEdges,
    groups,
    lifelines: [],
    notes: [],
    width: 0,
    height: 0,
    dropped,
  });
}

/** DFS back edges — the ones that would make ranking cyclic. Excluded from
 *  ranking, still drawn with their declared direction. */
function backEdges(ids: string[], edges: DiagramEdge[]): Set<DiagramEdge> {
  const out = new Map<string, DiagramEdge[]>();
  for (const e of edges) out.set(e.from, [...(out.get(e.from) ?? []), e]);
  const state = new Map<string, 0 | 1 | 2>();
  const found = new Set<DiagramEdge>();
  const visit = (id: string) => {
    state.set(id, 1);
    for (const e of out.get(id) ?? []) {
      const s = state.get(e.to) ?? 0;
      if (s === 1) found.add(e);
      else if (s === 0) visit(e.to);
    }
    state.set(id, 2);
  };
  for (const id of ids) if ((state.get(id) ?? 0) === 0) visit(id);
  return found;
}

/** Longest-path ranking over the acyclic remainder. */
function rankNodes(ids: string[], forward: DiagramEdge[]): Map<string, number> {
  const rank = new Map<string, number>(ids.map((id) => [id, 0]));
  const indegree = new Map<string, number>(ids.map((id) => [id, 0]));
  const out = new Map<string, DiagramEdge[]>();
  for (const e of forward) {
    out.set(e.from, [...(out.get(e.from) ?? []), e]);
    indegree.set(e.to, (indegree.get(e.to) ?? 0) + 1);
  }
  const queue = ids.filter((id) => indegree.get(id) === 0);
  for (let head = 0; head < queue.length; head++) {
    const id = queue[head];
    for (const e of out.get(id) ?? []) {
      rank.set(e.to, Math.max(rank.get(e.to)!, rank.get(id)! + 1));
      const left = indegree.get(e.to)! - 1;
      indegree.set(e.to, left);
      if (left === 0) queue.push(e.to);
    }
  }
  return rank;
}

/** Honour an `at` row hint only while it keeps every incident edge
 *  rank-increasing — a hint must never invert the graph it describes. */
function applyHints(spec: FlowSpec, rank: Map<string, number>, forward: DiagramEdge[]) {
  for (const n of spec.nodes) {
    if (!n.at) continue;
    const previous = rank.get(n.id)!;
    rank.set(n.id, n.at[1]);
    const consistent = forward.every(
      (e) =>
        (e.from !== n.id && e.to !== n.id) || rank.get(e.from)! < rank.get(e.to)!,
    );
    if (!consistent) rank.set(n.id, previous);
  }
}

/** Drop ranks left empty by a hint so the diagram has no blank bands. */
function compactRanks(rank: Map<string, number>) {
  const used = [...new Set(rank.values())].sort((a, b) => a - b);
  const dense = new Map(used.map((r, i) => [r, i] as const));
  for (const [id, r] of rank) rank.set(id, dense.get(r)!);
}

function orderRanks(cells: Map<string, Cell>, chains: string[][]): Cell[][] {
  const depth = Math.max(0, ...[...cells.values()].map((c) => c.rank)) + 1;
  const ranks: Cell[][] = Array.from({ length: depth }, () => []);
  for (const c of cells.values()) ranks[c.rank].push(c);

  const successors = new Map<string, string[]>();
  const predecessors = new Map<string, string[]>();
  for (const chain of chains) {
    for (let i = 0; i + 1 < chain.length; i++) {
      successors.set(chain[i], [...(successors.get(chain[i]) ?? []), chain[i + 1]]);
      predecessors.set(chain[i + 1], [...(predecessors.get(chain[i + 1]) ?? []), chain[i]]);
    }
  }

  for (let sweep = 0; sweep < ORDER_SWEEPS; sweep++) {
    const down = sweep % 2 === 0;
    const order = down
      ? [...ranks.keys()].slice(1)
      : [...ranks.keys()].slice(0, -1).reverse();
    for (const r of order) {
      const fixed = position(ranks[down ? r - 1 : r + 1]);
      const neighbours = down ? predecessors : successors;
      ranks[r] = byMedian(ranks[r], fixed, neighbours);
      ranks[r] = transpose(ranks[r], fixed, neighbours);
    }
  }
  return ranks.map(compactGroups);
}

const position = (cells: Cell[]): Map<string, number> =>
  new Map(cells.map((c, i) => [c.key, i] as const));

function byMedian(
  cells: Cell[],
  fixed: Map<string, number>,
  neighbours: Map<string, string[]>,
): Cell[] {
  const keyed = cells.map((c, i) => {
    const ps = (neighbours.get(c.key) ?? [])
      .map((k) => fixed.get(k))
      .filter((p): p is number => p !== undefined)
      .sort((a, b) => a - b);
    // A cell with no neighbour in the fixed rank keeps its current slot.
    return { c, m: ps.length ? median(ps) : i };
  });
  return keyed.sort((a, b) => a.m - b.m).map((k) => k.c);
}

function crossings(
  cells: Cell[],
  fixed: Map<string, number>,
  neighbours: Map<string, string[]>,
): number {
  const pairs: [number, number][] = [];
  for (const [i, c] of cells.entries()) {
    for (const k of neighbours.get(c.key) ?? []) {
      const p = fixed.get(k);
      if (p !== undefined) pairs.push([i, p]);
    }
  }
  let total = 0;
  for (let i = 0; i < pairs.length; i++) {
    for (let j = i + 1; j < pairs.length; j++) {
      const [ai, ap] = pairs[i];
      const [bi, bp] = pairs[j];
      if ((ai < bi && ap > bp) || (ai > bi && ap < bp)) total++;
    }
  }
  return total;
}

/** Adjacent-swap improvement — breaks the ties the median heuristic leaves. */
function transpose(
  cells: Cell[],
  fixed: Map<string, number>,
  neighbours: Map<string, string[]>,
): Cell[] {
  let best = [...cells];
  let bestScore = crossings(best, fixed, neighbours);
  for (let round = 0; round < TRANSPOSE_ROUNDS && bestScore > 0; round++) {
    let improved = false;
    for (let i = 0; i + 1 < best.length; i++) {
      const swapped = [...best];
      [swapped[i], swapped[i + 1]] = [swapped[i + 1], swapped[i]];
      const score = crossings(swapped, fixed, neighbours);
      if (score < bestScore) {
        best = swapped;
        bestScore = score;
        improved = true;
      }
    }
    if (!improved) break;
  }
  return best;
}

/** Make same-group cells contiguous, anchored at the group's first member so
 *  the crossing-minimised order survives as far as possible. */
function compactGroups(cells: Cell[]): Cell[] {
  const out: Cell[] = [];
  const taken = new Set<number>();
  for (const [i, cell] of cells.entries()) {
    if (taken.has(i)) continue;
    out.push(cell);
    taken.add(i);
    if (!cell.group) continue;
    for (let j = i + 1; j < cells.length; j++) {
      if (!taken.has(j) && cells[j].group === cell.group) {
        out.push(cells[j]);
        taken.add(j);
      }
    }
  }
  return out;
}

const gapBetween = (a: Cell, b: Cell): number =>
  NODE_GAP + (a.group !== b.group && (a.group || b.group) ? 2 * GROUP_PAD : 0);

function assignCross(ranks: Cell[][], chains: string[][], cells: Map<string, Cell>) {
  for (const rank of ranks) {
    let cursor = 0;
    for (const [i, c] of rank.entries()) {
      if (i > 0) cursor += gapBetween(rank[i - 1], c);
      c.cross = cursor + c.crossSize / 2;
      cursor += c.crossSize;
    }
  }

  const linked = new Map<string, string[]>();
  for (const chain of chains) {
    for (let i = 0; i + 1 < chain.length; i++) {
      linked.set(chain[i], [...(linked.get(chain[i]) ?? []), chain[i + 1]]);
      linked.set(chain[i + 1], [...(linked.get(chain[i + 1]) ?? []), chain[i]]);
    }
  }

  // Pull each cell toward its neighbours' median, clamped so the rank's order
  // and gaps still hold. Straightens long chains without reshuffling anything.
  for (let pass = 0; pass < REFINE_PASSES; pass++) {
    for (const rank of ranks) {
      for (const [i, c] of rank.entries()) {
        const xs = (linked.get(c.key) ?? [])
          .map((k) => cells.get(k)?.cross)
          .filter((v): v is number => v !== undefined)
          .sort((a, b) => a - b);
        if (!xs.length) continue;
        const previous = rank[i - 1];
        const next = rank[i + 1];
        const low = previous
          ? previous.cross + previous.crossSize / 2 + gapBetween(previous, c) + c.crossSize / 2
          : -Infinity;
        const high = next
          ? next.cross - next.crossSize / 2 - gapBetween(c, next) - c.crossSize / 2
          : Infinity;
        if (low > high) continue;
        c.cross = Math.min(Math.max(median(xs), low), high);
      }
    }
  }
}

function assignMain(ranks: Cell[][]) {
  let cursor = 0;
  for (const rank of ranks) {
    const band = rank.reduce((a, c) => Math.max(a, c.mainSize), 0);
    for (const c of rank) c.main = cursor + band / 2;
    cursor += band + RANK_GAP;
  }
}

/** Project the abstract frame onto x/y and write it back into the node boxes. */
function placeCells(
  cells: Map<string, Cell>,
  direction: "down" | "right",
): { crossMax: number } {
  let crossMax = 0;
  for (const c of cells.values()) {
    crossMax = Math.max(crossMax, c.cross + c.crossSize / 2);
    if (!c.node) continue;
    const { x, y } = toXY(c.main, c.cross, direction);
    c.node.x = x - c.node.w / 2;
    c.node.y = y - c.node.h / 2;
  }
  return { crossMax };
}

const toXY = (main: number, cross: number, direction: "down" | "right") =>
  direction === "down" ? { x: cross, y: main } : { x: main, y: cross };

function groupBoxes(nodes: LaidOutNode[]): LaidOutGroup[] {
  const names = [...new Set(nodes.map((n) => n.group).filter((g): g is string => !!g))];
  const boxes = names.map((label) => {
    const members = nodes.filter((n) => n.group === label);
    const x = Math.min(...members.map((n) => n.x)) - GROUP_PAD;
    const y = Math.min(...members.map((n) => n.y)) - GROUP_PAD - GROUP_LABEL_H;
    const right = Math.max(...members.map((n) => n.x + n.w)) + GROUP_PAD;
    const bottom = Math.max(...members.map((n) => n.y + n.h)) + GROUP_PAD;
    return { label, lane: members[0].lane, x, y, w: right - x, h: bottom - y };
  });
  // An overlapping cluster box is worse than none, so they go all or nothing.
  const overlap = boxes.some((a, i) =>
    boxes.some(
      (b, j) =>
        j > i && a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h,
    ),
  );
  return overlap ? [] : boxes;
}

function routeEdge(
  routed: Routed,
  cells: Map<string, Cell>,
  direction: "down" | "right",
  crossMax: number,
  index: number,
): LaidOutEdge {
  const { edge, key, mode } = routed;
  const chain = routed.chain.map((k) => cells.get(k)!);
  const points: Point[] = [];

  if (mode === "self") {
    const c = chain[0];
    const far = c.cross + c.crossSize / 2;
    const back = c.main - c.mainSize / 4;
    const front = c.main + c.mainSize / 4;
    points.push(
      pointOf(back, far, direction),
      pointOf(back, far + SELF_LOOP, direction),
      pointOf(front, far + SELF_LOOP, direction),
      pointOf(front, far, direction),
    );
  } else if (mode === "back") {
    // Route around the outside so a loop never crosses the boxes it links.
    const [a, b] = chain;
    const lane = crossMax + LOOP_OFFSET + index * LOOP_STEP;
    points.push(
      pointOf(a.main, a.cross + a.crossSize / 2, direction),
      pointOf(a.main, lane, direction),
      pointOf(b.main, lane, direction),
      pointOf(b.main, b.cross + b.crossSize / 2, direction),
    );
  } else {
    for (let i = 0; i + 1 < chain.length; i++) {
      const a = chain[i];
      const b = chain[i + 1];
      const exit = a.main + a.mainSize / 2;
      const enter = b.main - b.mainSize / 2;
      const bend = (exit + enter) / 2;
      if (i === 0) points.push(pointOf(exit, a.cross, direction));
      points.push(
        pointOf(bend, a.cross, direction),
        pointOf(bend, b.cross, direction),
        pointOf(enter, b.cross, direction),
      );
    }
  }

  const simplified = simplify(points);
  return {
    key,
    from: edge.from,
    to: edge.to,
    points: simplified,
    label: edge.label,
    labelAt: edge.label ? longestMidpoint(simplified) : undefined,
    style: edge.style,
    arrow: edge.arrow,
  };
}

const pointOf = (main: number, cross: number, direction: "down" | "right"): Point => {
  const { x, y } = toXY(main, cross, direction);
  return [x, y];
};

/** Drop duplicate and collinear waypoints so the path is the shortest polyline
 *  that still describes the same route. */
function simplify(points: Point[]): Point[] {
  const out: Point[] = [];
  for (const p of points) {
    const last = out[out.length - 1];
    if (last && last[0] === p[0] && last[1] === p[1]) continue;
    const prior = out[out.length - 2];
    if (
      prior &&
      last &&
      ((prior[0] === last[0] && last[0] === p[0]) || (prior[1] === last[1] && last[1] === p[1]))
    ) {
      out[out.length - 1] = p;
      continue;
    }
    out.push(p);
  }
  return out;
}

function longestMidpoint(points: Point[]): Point | undefined {
  let best: Point | undefined;
  let bestLength = -1;
  for (let i = 0; i + 1 < points.length; i++) {
    const [ax, ay] = points[i];
    const [bx, by] = points[i + 1];
    const length = Math.abs(bx - ax) + Math.abs(by - ay);
    if (length > bestLength) {
      bestLength = length;
      best = [(ax + bx) / 2, (ay + by) / 2];
    }
  }
  return best;
}

function sequenceLayout(spec: SequenceSpec, m: LayoutMetrics): DiagramLayout {
  const known = new Set(spec.participants.map((p) => p.id));
  const dropped: string[] = [];

  const lanes = assignLanes(spec.participants.map((p) => p.id));
  const lifelines: LaidOutLifeline[] = [];
  let cursor = 0;
  for (const p of spec.participants) {
    const lines = wrapLabel(p.label, WRAP_CHARS);
    const { w, h } = textBox(lines, m, HEAD_PAD_X, HEAD_PAD_Y, MIN_COL_W);
    lifelines.push({
      id: p.id,
      lane: lanes.get(p.id),
      label: p.label,
      lines,
      x: cursor,
      y: 0,
      w,
      h,
      bottom: 0,
    });
    cursor += w + COL_GAP;
  }
  const centre = new Map(lifelines.map((l) => [l.id, l.x + l.w / 2] as const));
  const headBottom = lifelines.reduce((a, l) => Math.max(a, l.h), 0);

  const edges: LaidOutEdge[] = [];
  const notes: LaidOutNote[] = [];
  let y = headBottom;
  for (const [i, step] of spec.steps.entries()) {
    if (!known.has(step.from) || !known.has(step.to)) {
      dropped.push(`${step.from} → ${step.to}`);
      continue;
    }
    const self = step.from === step.to;
    y += self ? SELF_STEP_GAP : STEP_GAP;
    const from = centre.get(step.from)!;
    const to = centre.get(step.to)!;
    const points: Point[] = self
      ? [
          [from, y - SELF_STEP_GAP / 3],
          [from + SELF_LOOP, y - SELF_STEP_GAP / 3],
          [from + SELF_LOOP, y],
          [from, y],
        ]
      : [
          [from, y],
          [to, y],
        ];
    edges.push({
      key: `${step.from}|${step.to}|${i}`,
      from: step.from,
      to: step.to,
      points,
      label: step.label,
      labelAt: step.label ? longestMidpoint(points) : undefined,
      style: step.style,
      arrow: "to",
    });
    if (step.note) {
      const lines = wrapLabel(step.note, WRAP_CHARS * 2);
      const { w, h } = textBox(lines, m, NOTE_PAD, NOTE_PAD, MIN_COL_W);
      const top = y + NOTE_GAP / 3;
      notes.push({ lines, x: Math.min(from, to), y: top, w, h });
      // Reserve the note's real height — a fixed gap would let a tall note
      // collide with the next message.
      y = top + h;
    }
  }

  const bottom = y + LIFELINE_TAIL;
  for (const l of lifelines) l.bottom = bottom;

  return normalize({
    kind: "sequence",
    nodes: [],
    edges,
    groups: [],
    lifelines,
    notes,
    width: 0,
    height: 0,
    dropped,
  });
}

/** Translate everything to a `PAD` margin and record the extents. */
function normalize(layout: DiagramLayout): DiagramLayout {
  const xs: number[] = [];
  const ys: number[] = [];
  const track = (x: number, y: number, w = 0, h = 0) => {
    xs.push(x, x + w);
    ys.push(y, y + h);
  };
  for (const n of layout.nodes) track(n.x, n.y, n.w, n.h);
  for (const g of layout.groups) track(g.x, g.y, g.w, g.h);
  for (const l of layout.lifelines) {
    track(l.x, l.y, l.w, l.h);
    track(l.x, l.bottom);
  }
  for (const n of layout.notes) track(n.x, n.y, n.w, n.h);
  for (const e of layout.edges) for (const [x, y] of e.points) track(x, y);
  if (!xs.length) return { ...layout, width: 2 * PAD, height: 2 * PAD };

  const dx = PAD - Math.min(...xs);
  const dy = PAD - Math.min(...ys);
  const shift = <T extends { x: number; y: number }>(o: T): T => ({ ...o, x: o.x + dx, y: o.y + dy });
  return {
    ...layout,
    nodes: layout.nodes.map(shift),
    groups: layout.groups.map(shift),
    notes: layout.notes.map(shift),
    lifelines: layout.lifelines.map((l) => ({ ...shift(l), bottom: l.bottom + dy })),
    edges: layout.edges.map((e) => ({
      ...e,
      points: e.points.map(([x, y]): Point => [x + dx, y + dy]),
      labelAt: e.labelAt ? ([e.labelAt[0] + dx, e.labelAt[1] + dy] as Point) : undefined,
    })),
    width: Math.round(Math.max(...xs) - Math.min(...xs) + 2 * PAD),
    height: Math.round(Math.max(...ys) - Math.min(...ys) + 2 * PAD),
  };
}

export function layoutDiagram(spec: DiagramSpec, metrics: LayoutMetrics): DiagramLayout {
  return spec.kind === "flow" ? flowLayout(spec, metrics) : sequenceLayout(spec, metrics);
}

/** A one-line description for `aria-label` — what the diagram is and its spine. */
export function diagramAriaLabel(spec: DiagramSpec, layout: DiagramLayout): string {
  const what = spec.kind === "flow" ? "Flow diagram" : "Sequence diagram";
  const title = spec.title ? `: ${spec.title}` : "";
  const size =
    spec.kind === "flow"
      ? `${layout.nodes.length} ${layout.nodes.length === 1 ? "step" : "steps"}`
      : `${layout.lifelines.length} participants, ${layout.edges.length} messages`;
  return `${what}${title}; ${size}`;
}

/** The diagram as sentences. A shape-only label cannot convey a graph, so this
 *  rides alongside it as the exact, screen-reader-legible view. */
export function diagramOutline(layout: DiagramLayout): string[] {
  const label = new Map<string, string>([
    ...layout.nodes.map((n) => [n.id, n.label] as const),
    ...layout.lifelines.map((l) => [l.id, l.label] as const),
  ]);
  const named = (id: string) => label.get(id) ?? id;
  return layout.edges.map((e) => {
    const arrow = e.arrow === "both" ? "↔" : "→";
    return `${named(e.from)} ${arrow} ${named(e.to)}${e.label ? ` (${e.label})` : ""}`;
  });
}
