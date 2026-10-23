import { describe, expect, it } from "vitest";
import type { LineageEvent } from "./lineage";
import { boardStats } from "./stats";
import { annotated, event, history } from "./testdata/annotated";
import { orders, records } from "./testdata/example";

const T0 = "2026-09-15T04:00:00Z";
const T1 = "2026-09-15T05:00:00Z";
const T2 = "2026-09-15T06:00:00Z";
const hoursAfter = (at: string, hours: number) => new Date(new Date(at).getTime() + hours * 3600 * 1000);

const held = event(T0, "merge/gate", { outcome: "human" });
const claimed = event(T0, "review/claimed");
const judged = event(T1, "review/overlap-judged", { headline: "#1702 covers #1698" });
const reviewed = event(T2, "review/finished", { verdict: "stage-reviewed" });
const blocked = event(T2, "review/finished", { verdict: "stage-review-blocked" });

const items = Object.entries({
  held: [held],
  judgedInReview: [claimed, judged],
  judgedWhileHeld: [held, judged],
  reviewedSinceJudged: [claimed, judged, reviewed],
  onlyJudged: [judged],
  inReview: [claimed],
  blocked: [claimed, blocked],
} satisfies Record<string, LineageEvent[]>).map(([id, events]) => ({ id, body: history(...events) }));

describe("boardStats", () => {
  it("counts each item on the board once per tile, a warn annotation as needing a person, and no annotation as a move", () => {
    expect(boardStats(annotated, items, {}, null, hoursAfter(T2, 1))).toEqual({
      inFlow: 3,
      done: 0,
      waiting: 3,
      healing: 0,
      halted: 1,
      moved: 6,
    });
    expect(boardStats(annotated, items, {}, null, hoursAfter(T2, 25)).moved).toBe(0);
  });

  it("reads the items as they stood at a replay moment", () => {
    const early = boardStats(annotated, items, {}, new Date("2026-09-15T04:30:00Z"), hoursAfter(T2, 1));
    expect(early).toEqual({ inFlow: 4, done: 0, waiting: 2, healing: 0, halted: 0, moved: 6 });
    expect(boardStats(annotated, items, {}, new Date("2026-09-15T05:30:00Z"), hoursAfter(T2, 1)).waiting).toBe(4);
  });

  it("counts only a placing event as a move, not an event the definition has no rule for", () => {
    const quiet = [{ id: "quiet", body: history(event("2026-09-13T04:00:00Z", "review/claimed"), event(T2, "review/follow-ups")) }];
    expect(boardStats(annotated, quiet, {}, null, hoursAfter(T2, 1))).toMatchObject({ inFlow: 1, moved: 0 });
  });
});

describe("boardStats on the Orders board", () => {
  const now = new Date("2026-09-15T06:20:00Z");
  const every = records.map(({ id, body }) => ({ id, body }));
  const shown = every.filter(({ id }) => id !== "ORD-2601" && id !== "ORD-2655");

  it("adds what a window hides to the tile of its lane, and to no tile for an exit", () => {
    const stats = boardStats(orders, shown, {}, null, now);
    expect(stats.done).toBe(12);
    expect(boardStats(orders, shown, { verified: 30 }, null, now)).toEqual({ ...stats, done: 42 });
    expect(boardStats(orders, shown, { deployed: 2, "not-code": 4 }, null, now)).toEqual({ ...stats, inFlow: stats.inFlow + 2 });
  });

  it("counts a replay from every record, with no window", () => {
    expect(boardStats(orders, every, {}, new Date("2026-08-29T00:00:00Z"), now)).toEqual({
      inFlow: 0,
      done: 1,
      waiting: 0,
      healing: 0,
      halted: 0,
      moved: 1,
    });
  });
});
