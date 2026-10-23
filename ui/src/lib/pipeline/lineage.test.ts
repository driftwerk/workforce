import { describe, expect, it } from "vitest";
import { type LineageBody, allEvents, enteredAt, lastPlacement, positionAt, standingAnnotation } from "./lineage";
import { annotated, event, history } from "./testdata/annotated";
import { orders, records } from "./testdata/example";

const claimed = event("2026-09-15T04:00:00Z", "review/claimed");
const judged = event("2026-09-15T05:00:00Z", "review/overlap-judged", { headline: "#1702 covers everything #1698 does" });
const followUps = event("2026-09-15T05:30:00Z", "review/follow-ups");
const resolved = event("2026-09-15T06:00:00Z", "watch/resolved");
const finished = event("2026-09-15T06:00:00Z", "review/finished", { verdict: "stage-reviewed" });

const at = (s: string) => new Date(s);

function record(id: string): LineageBody {
  const found = records.find((r) => r.id === id);
  if (!found) throw new Error(`no ${id} in the fixture`);
  return found.body;
}

describe("positionAt", () => {
  it("places every live record where the hub placed it", () => {
    for (const { id, column, body } of records) {
      expect(positionAt(orders, body, null)?.column, id).toBe(column);
    }
  });

  it("folds the history up to a replay moment", () => {
    const body = record("ORD-2797");
    expect(positionAt(orders, body, at("2026-09-14T18:30:00Z"))).toEqual({
      column: "deployed",
      flow: true,
      tone: "good",
      label: "Deployed",
      rung: undefined,
    });
    expect(positionAt(orders, body, at("2026-09-14T17:48:25Z"))?.column).toBe("human-merge");
    expect(positionAt(orders, body, at("2026-09-14T10:00:00Z"))).toBeNull();
  });

  it("falls back to the stage block when there is no history, on the live board only", () => {
    const body: LineageBody = { stage: "verify", stages: { verify: { last_event: "rung", state: "pass", rung: "6h" } } };
    expect(positionAt(orders, body, null)).toEqual({
      column: "verifying",
      flow: true,
      tone: "active",
      label: "Verifying",
      rung: "6h",
    });
    expect(positionAt(orders, body, at("2026-09-15T00:00:00Z"))).toBeNull();
  });
});

describe("enteredAt", () => {
  it("is when the last placing event was recorded", () => {
    const body = record("ORD-2797");
    expect(enteredAt(orders, body)).toBe("2026-09-15T04:53:20.286220Z");
    expect(enteredAt(orders, body, at("2026-09-14T17:48:26Z"))).toBe("2026-09-14T17:48:24.022657Z");
    expect(enteredAt(orders, body, at("2026-09-14T10:00:00Z"))).toBe(body.updated_at);
  });
});

describe("lastPlacement", () => {
  it("skips events the definition has no rule for", () => {
    const last = lastPlacement(orders, record("ORD-2797"), at("2026-09-14T17:48:26Z"));
    expect(last?.event.event).toBe("gate");
    expect(last?.rule.column).toBe("human-merge");
    expect(lastPlacement(orders, {}, null)).toBeNull();
  });
});

describe("annotations", () => {
  it("never place an item", () => {
    const body = history(claimed, judged);
    expect(lastPlacement(annotated, body, null)?.event).toBe(claimed);
    expect(positionAt(annotated, body, null)?.column).toBe("reviewing");
    expect(enteredAt(annotated, body)).toBe(claimed.at);
    for (const { id, column, body: live } of records) {
      expect(positionAt(annotated, live, null)?.column, id).toBe(column);
    }
  });
});

describe("standingAnnotation", () => {
  it("is the newest annotation since the item last moved", () => {
    expect(standingAnnotation(annotated, history(claimed, judged), null)).toEqual({
      tone: "warn",
      label: "#1702 covers everything #1698 does",
      at: judged.at,
    });
    expect(standingAnnotation(annotated, history(claimed, judged, resolved), null)).toEqual({
      tone: "good",
      label: "resolved",
      at: resolved.at,
    });
  });

  it("stands through an event the definition has no rule for and clears at the next placement", () => {
    expect(standingAnnotation(annotated, history(claimed, judged, followUps), null)?.at).toBe(judged.at);
    expect(standingAnnotation(annotated, history(claimed, judged, finished), null)).toBeNull();
    expect(standingAnnotation(annotated, history(claimed), null)).toBeNull();
  });

  it("reads the history as it stood at a replay moment", () => {
    const body = history(claimed, judged, finished);
    expect(standingAnnotation(annotated, body, at("2026-09-15T05:45:00Z"))?.at).toBe(judged.at);
    expect(standingAnnotation(annotated, body, at("2026-09-15T04:30:00Z"))).toBeNull();
  });

  it("names an annotation whose label field is empty by its event", () => {
    const blank = event("2026-09-15T05:00:00Z", "review/overlap-judged", { headline: " " });
    expect(standingAnnotation(annotated, history(claimed, blank), null)?.label).toBe("review/overlap-judged");
  });
});

describe("allEvents", () => {
  it("gives a placing event its position, an annotation its note, and an unruled event neither", () => {
    const byEvent = Object.fromEntries(
      allEvents(annotated, [{ id: "ORD-2813", body: history(claimed, judged, followUps) }]).map((e) => [e.event, e]),
    );
    expect(byEvent.claimed?.position?.column).toBe("reviewing");
    expect(byEvent.claimed?.annotation).toBeNull();
    expect(byEvent["overlap-judged"]?.position).toBeNull();
    expect(byEvent["overlap-judged"]?.annotation).toEqual({ tone: "warn", label: "#1702 covers everything #1698 does" });
    expect(byEvent["follow-ups"]?.position).toBeNull();
    expect(byEvent["follow-ups"]?.annotation).toBeNull();
  });
});
