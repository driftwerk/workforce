import { describe, expect, it } from "vitest";
import { formatDateTime } from "@/lib/utils";
import {
  type Activity,
  LEGACY_LEASE_MINUTES,
  type Ranked,
  activityAt,
  activityCue,
  activityDetail,
  byActivity,
} from "./activity";
import { type LineageBody, type LineageEvent, enteredAt, positionAt } from "./lineage";
import { orders, records } from "./testdata/example";

const at = (s: string) => new Date(s);

function event(time: string, key: string, detail: Record<string, unknown> = {}): LineageEvent {
  const [stage = "", name = ""] = key.split("/");
  return { at: time, stage, event: name, detail };
}

const history = (...events: LineageEvent[]): LineageBody => ({ history: events });

const started = event("2026-09-15T04:48:00Z", "verify/started", { lease_minutes: 150, rung: "1h" });

describe("activityAt", () => {
  it("holds a claim while its lease runs, the lease a number or numeric text", () => {
    expect(activityAt(orders, history(started), null, at("2026-09-15T05:00:00Z"))).toEqual({
      kind: "working",
      since: started.at,
      until: at("2026-09-15T07:18:00Z"),
    });
    const text = event(started.at, "verify/started", { lease_minutes: "150" });
    expect(activityAt(orders, history(text), null, at("2026-09-15T07:17:59Z")).kind).toBe("working");
  });

  it("makes a claim whose lease ran out due from when it was claimed", () => {
    expect(activityAt(orders, history(started), null, at("2026-09-15T07:18:00Z"))).toEqual({
      kind: "scheduled",
      due: at(started.at),
    });
  });

  it(`gives a claim that records no lease under a leased rule ${LEGACY_LEASE_MINUTES} minutes`, () => {
    for (const detail of [{ rung: "24h" }, { rung: "24h", lease_minutes: "soon" }]) {
      const legacy = event("2026-09-13T16:33:05Z", "verify/started", detail);
      expect(activityAt(orders, history(legacy), null, at("2026-09-13T17:00:00Z"))).toEqual({
        kind: "working",
        since: legacy.at,
        until: at("2026-09-13T17:18:05Z"),
      });
      expect(activityAt(orders, history(legacy), null, at("2026-09-15T04:55:00Z"))).toEqual({
        kind: "scheduled",
        due: at(legacy.at),
      });
    }
  });

  it("holds a claim under a rule with no lease until the item moves on", () => {
    const claimed = event("2026-09-14T13:00:03Z", "implement/claimed");
    expect(activityAt(orders, history(claimed), null, at("2026-09-20T00:00:00Z"))).toEqual({
      kind: "working",
      since: claimed.at,
      until: null,
    });
  });

  it("schedules an item whose event says when it is next due, past or future", () => {
    const now = at("2026-09-15T04:55:00Z");
    const overdue = event("2026-09-15T04:35:36Z", "verify/rung", { state: "pass", due_at: "2026-09-14T21:50:07Z" });
    expect(activityAt(orders, history(overdue), null, now)).toEqual({ kind: "scheduled", due: at("2026-09-14T21:50:07Z") });
    const later = event("2026-09-14T10:23:54Z", "verify/inconclusive", { final: "false", due_at: "2026-09-16T10:07:00Z" });
    expect(activityAt(orders, history(later), null, now)).toEqual({ kind: "scheduled", due: at("2026-09-16T10:07:00Z") });
  });

  it("reads a missing or unparseable due time, or one on a rule without a due field, as nothing scheduled", () => {
    const now = at("2026-09-15T04:55:00Z");
    for (const body of [
      history(event("2026-09-13T16:38:00Z", "verify/inconclusive", { final: "false" })),
      history(event("2026-09-13T16:38:00Z", "verify/inconclusive", { final: "false", due_at: "tomorrow" })),
      history(event("2026-09-13T16:38:00Z", "verify/inconclusive", { final: "true", due_at: "2026-09-16T10:07:00Z" })),
      history(event("2026-09-13T16:38:00Z", "merge/unknown", { due_at: "2026-09-16T10:07:00Z" })),
    ]) {
      expect(activityAt(orders, body, null, now)).toEqual({ kind: "other" });
    }
  });

  it("ends a claim at the next placement, but not at an event the definition has no rule for", () => {
    const now = at("2026-09-14T18:00:00Z");
    const claimed = event("2026-09-14T17:23:05Z", "review/claimed");
    const followUps = event("2026-09-14T17:48:25Z", "review/follow-ups");
    expect(activityAt(orders, history(claimed, followUps), null, now).kind).toBe("working");
    const finished = event("2026-09-14T17:48:26Z", "review/finished", { verdict: "stage-reviewed" });
    expect(activityAt(orders, history(claimed, followUps, finished), null, now)).toEqual({ kind: "other" });
  });

  it("reads a replay moment as the item stood then, leases and due times included", () => {
    const deployed = event("2026-09-14T18:28:35Z", "deploy/finished", {
      label: "stage-deployed",
      due_at: "2026-09-14T19:28:35Z",
    });
    const wallClock = at("2026-09-15T09:00:00Z");
    expect(activityAt(orders, history(deployed, started), at("2026-09-15T04:00:00Z"), wallClock)).toEqual({
      kind: "scheduled",
      due: at("2026-09-14T19:28:35Z"),
    });
    expect(activityAt(orders, history(deployed, started), at("2026-09-15T05:00:00Z"), wallClock).kind).toBe("working");
  });
});

describe("byActivity", () => {
  const moment = at("2026-09-15T04:55:00Z");
  const order = (id: string, replay: Date | null, now: Date) =>
    records
      .filter((record) => positionAt(orders, record.body, replay)?.column === id)
      .map(
        (record): Ranked => ({
          id: record.id,
          enteredAt: enteredAt(orders, record.body, replay),
          activity: activityAt(orders, record.body, replay, now),
        }),
      )
      .sort(byActivity)
      .map((record) => record.id);
  const column = (id: string) => order(id, moment, moment);

  it("keeps every live column in the same order across a tick of the board's clock", () => {
    const now = at("2026-09-15T06:20:00Z");
    const tick = new Date(now.getTime() + 30_000);
    expect(order("verifying", null, now).length).toBeGreaterThan(0);
    for (const { id } of [...orders.lanes, ...(orders.exits ?? [])]) {
      expect(order(id, null, tick), id).toEqual(order(id, null, now));
    }
  });

  it("moves a card only when its lease runs out", () => {
    const bodies: Record<string, LineageBody> = {
      LEASED: history(started),
      OVERDUE: history(event("2026-09-15T04:40:00Z", "verify/rung", { state: "pass", due_at: "2026-09-15T04:00:00Z" })),
    };
    const ranked = (now: Date) =>
      Object.entries(bodies)
        .map(([id, body]): Ranked => ({ id, activity: activityAt(orders, body, null, now) }))
        .sort(byActivity)
        .map((r) => r.id);
    expect(ranked(at("2026-09-15T07:17:30Z"))).toEqual(["LEASED", "OVERDUE"]);
    expect(ranked(at("2026-09-15T07:17:59Z"))).toEqual(["LEASED", "OVERDUE"]);
    expect(ranked(at("2026-09-15T07:18:29Z"))).toEqual(["OVERDUE", "LEASED"]);
  });

  it("orders Verifying: in flight, then most overdue, then due soonest, then what has no due time", () => {
    expect(column("verifying")).toEqual([
      "ORD-2811",
      "ORD-2802",
      "ORD-2806",
      "ORD-2804",
      "ORD-2797",
      "ORD-2764",
      "ORD-2785",
      "ORD-2759",
      "ORD-2778",
      "ORD-2761",
      "ORD-2775",
      "ORD-2787",
      "ORD-2774",
      "ORD-2773",
      "ORD-2779",
      "ORD-2791",
      "ORD-2762",
      "ORD-2784",
      "ORD-2763",
      "ORD-2757",
      "ORD-2758",
    ]);
  });

  it("puts the most overdue first rung on top of Deployed", () => {
    expect(column("deployed")).toEqual(["ORD-2805", "ORD-2807", "ORD-2803", "ORD-2796"]);
  });

  it("orders claims newest first and the rest newest arrival first, ties by id", () => {
    const other: Activity = { kind: "other" };
    const ranked: Ranked[] = [
      { id: "B", enteredAt: "2026-09-14T10:00:00Z", activity: other },
      { id: "OLD", enteredAt: "2026-09-10T00:00:00Z", activity: { kind: "working", since: "2026-09-10T00:00:00Z", until: null } },
      { id: "C", enteredAt: "2026-09-15T10:00:00Z", activity: other },
      { id: "A", enteredAt: "2026-09-14T11:00:00+01:00", activity: other },
      { id: "NEW", enteredAt: "2026-09-11T00:00:00Z", activity: { kind: "working", since: "2026-09-11T00:00:00Z", until: null } },
      { id: "D", activity: other },
    ];
    expect([...ranked].sort(byActivity).map((r) => r.id)).toEqual(["NEW", "OLD", "C", "A", "B", "D"]);
  });
});

describe("activityCue", () => {
  const t = at("2026-09-15T04:55:00Z");
  const due = (ms: number): Activity => ({ kind: "scheduled", due: new Date(t.getTime() + ms) });

  it("says a leased claim is working now and a leaseless one when it was claimed", () => {
    expect(activityCue({ kind: "working", since: "2026-09-15T04:53:01Z", until: at("2026-09-15T07:23:01Z") }, t)).toBe(
      "working now",
    );
    expect(activityCue({ kind: "working", since: "2026-09-15T01:55:00Z", until: null }, t)).toBe("claimed 3h ago");
  });

  it("says how overdue or how soon an item is due", () => {
    expect(activityCue(due(-30_000), t)).toBe("due now");
    expect(activityCue(due(30_000), t)).toBe("due now");
    expect(activityCue(due(-(6 * 3_600_000 + 55 * 60_000)), t)).toBe("overdue by 6h 55m");
    expect(activityCue(due(15 * 3_600_000 + 40 * 60_000), t)).toBe("due in 15h 40m");
    expect(activityCue(due(29 * 3_600_000 + 10 * 60_000), t)).toBe("due in 1d 5h");
  });

  it("says nothing when nothing is scheduled", () => {
    expect(activityCue({ kind: "other" }, t)).toBeNull();
  });
});

describe("activityDetail", () => {
  it("names the lease's end or the due time", () => {
    const until = at("2026-09-15T07:23:00Z");
    expect(activityDetail({ kind: "working", since: "2026-09-15T04:53:00Z", until })).toBe(
      `lease until ${formatDateTime(until.toISOString())}`,
    );
    expect(activityDetail({ kind: "working", since: "2026-09-15T04:53:00Z", until: null })).toBeNull();
    expect(activityDetail({ kind: "scheduled", due: until })).toBe(`next due ${formatDateTime(until.toISOString())}`);
    expect(activityDetail({ kind: "other" })).toBeNull();
  });
});
