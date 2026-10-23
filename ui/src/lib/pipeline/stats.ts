import type { PipelineDefinition } from "./definition";
import { type LineageBody, allEvents, positionAt, standingAnnotation } from "./lineage";

/** The board's headline counts: each of the items on the board at its moment, counted once per tile. */
export interface BoardStats {
  /** In a lane other than the last, including those a window hides. */
  inFlow: number;
  /** In the last lane, including those a window hides. */
  done: number;
  /** Placed with a `warn` tone, or carrying a `warn` annotation since they last moved: a person's to act on. */
  waiting: number;
  /** Placed with a `healing` tone: the pipeline is still fixing it itself. */
  healing: number;
  /** Placed with a `bad` tone. */
  halted: number;
  /** Items an event placed in the 24 hours up to the board's moment; an annotation moves nothing. */
  moved: number;
}

const DAY_MS = 24 * 3600 * 1000;

/**
 * The counts at time `at` (null = now, read at `now`), from each item's
 * history folded up to that moment, with `hidden`, the items a column's
 * window leaves off the board, added to the column tiles. A hidden item is
 * finished and needs nobody, so the tone tiles count only what is shown.
 */
export function boardStats(
  def: PipelineDefinition,
  items: { id: string; body: LineageBody }[],
  hidden: Record<string, number>,
  at: Date | null,
  now: Date,
): BoardStats {
  const lanes = def.lanes.map((lane) => lane.id);
  const lastLane = lanes[lanes.length - 1];
  const hiddenIn = (columns: string[]) => columns.reduce((sum, column) => sum + (hidden[column] ?? 0), 0);
  const placed = items.flatMap(({ body }) => {
    const position = positionAt(def, body, at);
    return position ? [{ position, annotation: standingAnnotation(def, body, at) }] : [];
  });
  const count = (holds: (item: (typeof placed)[number]) => boolean) => placed.filter(holds).length;
  const since = (at ?? now).getTime() - DAY_MS;
  return {
    inFlow: count(({ position }) => position.flow && position.column !== lastLane) + hiddenIn(lanes.slice(0, -1)),
    done: count(({ position }) => position.column === lastLane) + hiddenIn(lanes.slice(-1)),
    waiting: count(({ position, annotation }) => position.tone === "warn" || annotation?.tone === "warn"),
    healing: count(({ position }) => position.tone === "healing"),
    halted: count(({ position }) => position.tone === "bad"),
    moved: new Set(
      allEvents(def, items)
        .filter((e) => e.position !== null && new Date(e.at).getTime() >= since && (!at || new Date(e.at) <= at))
        .map((e) => e.item),
    ).size,
  };
}
