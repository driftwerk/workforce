import { describe, expect, it } from "vitest";
import type { ItemTimings, TimingCoverage, Visit } from "@/lib/api/pipelines";
import { formatDateTime } from "@/lib/utils";
import {
  durationText,
  live,
  milestoneReadings,
  originText,
  timingNotes,
  timingsErrorText,
  visitLabel,
  workShare,
} from "./timings";

const MINUTE = 60_000;

const covered: TimingCoverage = {
  runs_named: 8,
  runs_timed: 8,
  runs_unmatched: 0,
  runs_by_lifetime: 0,
  work_unavailable: false,
  origin_recorded: false,
  origin_after_start: false,
  history_truncated: false,
  clock_adjusted: false,
};

const deployed: Visit = {
  kind: "column",
  column: "deployed",
  label: "Deployed",
  entered_at: "2026-09-14T18:28:35Z",
  left_at: "2026-09-15T04:48:00Z",
  elapsed_ms: 37_165_000,
  work_ms: 0,
  waiting_ms: 37_165_000,
};

const verifying: Visit = {
  kind: "column",
  column: "verifying",
  label: "Verifying",
  entered_at: "2026-09-15T04:48:00Z",
  left_at: null,
  elapsed_ms: 7 * MINUTE,
  work_ms: 5 * MINUTE,
  waiting_ms: 2 * MINUTE,
};

const ord2797: ItemTimings = {
  pipeline: "orders",
  scope: "agent:example",
  item: "ORD-2797",
  as_of: "2026-09-15T04:55:00Z",
  origin: { at: "2026-09-14T10:15:03Z", label: "Filed", recorded: false },
  started_at: "2026-09-14T10:15:03Z",
  finished_at: null,
  total: {
    elapsed_ms: 67_197_000,
    in_pipeline_ms: 67_197_000,
    work_ms: 6_000_000,
    waiting_ms: 61_197_000,
    open: true,
    working: false,
  },
  milestones: [
    { column: "deployed", label: "Deployed", reached_at: "2026-09-14T18:28:35Z", from_origin_ms: 29_612_000 },
    { column: "verified", label: "Verified", reached_at: null, from_origin_ms: null },
  ],
  visits: [deployed, verifying],
  columns: [
    { column: "deployed", label: "Deployed", visits: 1, elapsed_ms: 37_165_000, work_ms: 0, waiting_ms: 37_165_000 },
    { column: "verifying", label: "Verifying", visits: 1, elapsed_ms: 7 * MINUTE, work_ms: 5 * MINUTE, waiting_ms: 2 * MINUTE },
  ],
  runs: [],
  coverage: covered,
};

const aMinuteLater = new Date("2026-09-15T04:56:00Z");

describe("live", () => {
  it("advances an idle open item's elapsed and waiting", () => {
    const t = live(ord2797, aMinuteLater);
    expect(t.total).toEqual({
      ...ord2797.total,
      elapsed_ms: ord2797.total.elapsed_ms + MINUTE,
      in_pipeline_ms: ord2797.total.in_pipeline_ms + MINUTE,
      waiting_ms: 61_197_000 + MINUTE,
    });
    expect(t.visits[0]).toBe(deployed);
    expect(t.visits[1]).toEqual({ ...verifying, elapsed_ms: 8 * MINUTE, waiting_ms: 3 * MINUTE });
    expect(t.columns[0]).toBe(ord2797.columns[0]);
    expect(t.columns[1]).toMatchObject({ elapsed_ms: 8 * MINUTE, work_ms: 5 * MINUTE, waiting_ms: 3 * MINUTE });
  });

  it("advances a worked item's elapsed and work", () => {
    const t = live({ ...ord2797, total: { ...ord2797.total, working: true } }, aMinuteLater);
    expect(t.total).toMatchObject({ work_ms: 6_000_000 + MINUTE, waiting_ms: 61_197_000 });
    expect(t.visits[1]).toMatchObject({ elapsed_ms: 8 * MINUTE, work_ms: 6 * MINUTE, waiting_ms: 2 * MINUTE });
  });

  it("leaves unknown work unknown", () => {
    const unknown = { ...verifying, work_ms: null, waiting_ms: null };
    const t = live(
      { ...ord2797, total: { ...ord2797.total, work_ms: null, waiting_ms: null }, visits: [unknown], columns: [] },
      aMinuteLater,
    );
    expect(t.total).toMatchObject({ elapsed_ms: ord2797.total.elapsed_ms + MINUTE, work_ms: null, waiting_ms: null });
    expect(t.visits[0]).toMatchObject({ elapsed_ms: 8 * MINUTE, work_ms: null, waiting_ms: null });
  });

  it("leaves a finished item, or a clock behind the reading, as it is", () => {
    const finished = { ...ord2797, total: { ...ord2797.total, open: false } };
    expect(live(finished, aMinuteLater)).toBe(finished);
    expect(live(ord2797, new Date("2026-09-15T04:54:00Z"))).toBe(ord2797);
  });
});

describe("timingNotes", () => {
  const notes = (coverage: Partial<TimingCoverage>, origin = ord2797.origin) =>
    timingNotes({ ...ord2797, origin, coverage: { ...covered, origin_recorded: true, ...coverage } }, "ticket").map(
      (note) => [note.text, note.warn] as const,
    );

  it("says nothing when the figures cover everything", () => {
    expect(notes({})).toEqual([]);
  });

  it("warns about runs that could not be matched, singular and plural", () => {
    expect(notes({ runs_unmatched: 1 })).toEqual([
      [
        "1 run this ticket names could not be matched to a workflow run this pipeline counts, so its time shows as waiting",
        true,
      ],
    ]);
    expect(notes({ runs_unmatched: 3 })[0]?.[0]).toMatch(/^3 runs .* so their time shows as waiting$/);
  });

  it("says why each other figure may be short", () => {
    expect(notes({ work_unavailable: true })).toEqual([
      ["Work and waiting are not shown: telemetry is not configured on this hub", false],
    ]);
    expect(notes({ origin_recorded: false })).toEqual([
      ["No filed time is recorded for this ticket, so times run from when the pipeline first recorded it", false],
    ]);
    expect(notes({ origin_recorded: false }, { ...ord2797.origin, label: null })).toEqual([]);
    expect(notes({ origin_after_start: true })[0]?.[0]).toBe(
      "The recorded filed time is later than when the pipeline first recorded this ticket, so times run from the first record",
    );
    expect(notes({ runs_by_lifetime: 1 })[0]?.[0]).toBe("1 run has no step history, so its whole run counts as work");
    expect(notes({ runs_by_lifetime: 2 })[0]?.[0]).toBe("2 runs have no step history, so their whole run counts as work");
    expect(notes({ history_truncated: true })[0]?.[0]).toBe("Older history was trimmed; its time is shown as one stretch");
    expect(notes({ clock_adjusted: true })[0]?.[0]).toMatch(/^Some times were adjusted/);
  });
});

describe("readings", () => {
  it("labels each kind of stretch", () => {
    expect(visitLabel(verifying)).toBe("Verifying");
    const leading = { ...verifying, column: null, label: null };
    expect(visitLabel({ ...leading, kind: "before_pipeline" })).toBe("Before the pipeline");
    expect(visitLabel({ ...leading, kind: "trimmed" })).toBe("Earlier history (trimmed)");
    expect(visitLabel({ ...leading, kind: "unplaced" })).toBe("Not yet on the board");
  });

  it("names the origin only when it was recorded", () => {
    expect(originText(ord2797)).toBe("First recorded");
    expect(originText({ ...ord2797, origin: { ...ord2797.origin, recorded: true } })).toBe("Filed");
  });

  it("reads milestones as reached, not yet or never", () => {
    expect(milestoneReadings(ord2797)).toEqual([
      { label: "First recorded → Deployed", value: "8h 13m", hint: formatDateTime("2026-09-14T18:28:35Z") },
      { label: "First recorded → Verified", value: "Not yet", hint: "18h 39m so far" },
    ]);
    const finished = { ...ord2797, total: { ...ord2797.total, open: false } };
    expect(milestoneReadings(finished)[1]).toEqual({ label: "First recorded → Verified", value: "Never", hint: null });
  });

  it("writes durations, dashes for unknown and 0s for none", () => {
    expect(durationText(null)).toBe("—");
    expect(durationText(0)).toBe("0s");
    expect(durationText(400)).toBe("0s");
    expect(durationText(90_000)).toBe("1m 30s");
  });

  it("measures a column's share of time working", () => {
    expect(workShare(ord2797.columns[1]!)).toBeCloseTo(5 / 7);
    expect(workShare({ ...ord2797.columns[1]!, elapsed_ms: 0, work_ms: 0 })).toBeNull();
    expect(workShare({ ...ord2797.columns[1]!, work_ms: null })).toBeNull();
  });

  it("explains a failed request", () => {
    expect(timingsErrorText(Object.assign(new Error("gone"), { status: 404 }))).toBe(
      "Timings are unavailable: this item is no longer in the pipeline",
    );
    expect(timingsErrorText(Object.assign(new Error("no"), { status: 403 }))).toBe("Timings are not visible to this login");
    expect(timingsErrorText(new Error('{"error":"pipeline request failed"}'))).toBe(
      "Timings could not be loaded: pipeline request failed",
    );
  });
});
