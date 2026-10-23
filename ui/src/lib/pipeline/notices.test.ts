import { describe, expect, it } from "vitest";
import type { PipelineNotice } from "@/lib/api/pipelines";
import { afterDismissing, dismissedKey, noticeToken, parseDismissed, safeLink, shownNotices } from "./notices";

function notice(key: string, raised_at: string, link: string | null = null): PipelineNotice {
  return { key, tone: "warn", title: key, detail: null, link, raised_at, expires_at: null };
}

const runner = notice("runner-protocol", "2026-09-15T12:02:00Z");
const grant = notice("stale-grant", "2026-09-15T12:05:00Z");

describe("shownNotices", () => {
  it("hides what this viewer dismissed and shows a notice raised again", () => {
    expect(dismissedKey("agent:example", "orders")).toBe("pipeline-notices-dismissed:agent:example/orders");
    expect(noticeToken(runner)).toBe("runner-protocol@2026-09-15T12:02:00Z");
    expect(shownNotices([runner, grant], [noticeToken(runner)])).toEqual([grant]);
    const raisedAgain = notice("runner-protocol", "2026-09-15T13:00:00Z");
    expect(shownNotices([raisedAgain, grant], [noticeToken(runner)])).toEqual([raisedAgain, grant]);
    expect(shownNotices([], [noticeToken(runner)])).toEqual([]);
  });
});

describe("afterDismissing", () => {
  it("adds the notice once and forgets dismissals of notices no longer raised", () => {
    const stale = "gone@2026-09-01T00:00:00Z";
    expect(afterDismissing([runner, grant], [stale, noticeToken(runner)], grant)).toEqual([noticeToken(runner), noticeToken(grant)]);
    expect(afterDismissing([runner], [noticeToken(runner)], runner)).toEqual([noticeToken(runner)]);
  });
});

describe("parseDismissed", () => {
  it("reads a stored list and tolerates anything else", () => {
    expect(parseDismissed('["a@1","b@2"]')).toEqual(["a@1", "b@2"]);
    expect(parseDismissed('["a@1",2,null]')).toEqual(["a@1"]);
    for (const raw of [null, "", "not json", '{"a":1}', "7"]) {
      expect(parseDismissed(raw), String(raw)).toEqual([]);
    }
  });
});

describe("safeLink", () => {
  it("keeps absolute http(s) links and paths on this site", () => {
    for (const link of ["https://github.com/acme/app/pull/1698", "http://localhost:3000/x", "/pipelines/agent:example/orders"]) {
      expect(safeLink(link)).toBe(link);
    }
  });

  it("refuses every other scheme, and a path the browser would take to another host", () => {
    for (const link of [
      "javascript:alert(1)",
      "JAVASCRIPT:alert(1)",
      "ftp://host/x",
      "//evil.example",
      "/\\evil.example",
      "/\t/evil.example",
      "/\n/evil.example",
      "pipelines/x",
      "",
      null,
    ]) {
      expect(safeLink(link), String(link)).toBeNull();
    }
  });

  it("gives the link as the browser reads it", () => {
    expect(safeLink("/pipelines/agent:example/orders?item=ORD 1#history")).toBe("/pipelines/agent:example/orders?item=ORD%201#history");
    expect(safeLink("https://github.com")).toBe("https://github.com/");
  });
});
