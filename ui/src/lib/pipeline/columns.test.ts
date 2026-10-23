import { describe, expect, it } from "vitest";
import {
  CARD_REM,
  COLUMN_CHROME_REM,
  cardChoices,
  cardsPerColumn,
  columnHeight,
  emptyText,
  hasWindows,
  hiddenText,
  parseWindow,
  windowChoices,
  windowLabel,
  windowReach,
  windowsParam,
} from "./columns";
import { orders } from "./testdata/example";

describe("parseWindow", () => {
  it("reads each spelling the hub accepts, with its hours", () => {
    for (const [text, hours] of [
      ["24h", 24],
      ["36h", 36],
      ["7d", 168],
      ["168h", 168],
      ["30d", 720],
      ["3650d", 87_600],
      ["all", Infinity],
    ] as const) {
      expect(parseWindow(text), text).toEqual({ text, hours });
    }
  });

  it("refuses each spelling the hub refuses", () => {
    for (const text of ["0d", "23h", "7", "7w", "1.5d", "-1d", "07d", "3651d", "99999999999h", "", "ALL", " 7d"]) {
      expect(parseWindow(text), text).toBeNull();
    }
  });
});

describe("windowChoices", () => {
  it("offers 24h, 7d, 30d and all, with the definition's window in its own spelling, shortest first", () => {
    expect(windowChoices("7d")).toEqual(["24h", "7d", "30d", "all"]);
    expect(windowChoices("14d")).toEqual(["24h", "7d", "14d", "30d", "all"]);
    expect(windowChoices("168h")).toEqual(["24h", "168h", "30d", "all"]);
    expect(windowChoices("all")).toEqual(["24h", "7d", "30d", "all"]);
  });
});

describe("windowsParam", () => {
  it("asks for the definition's windows when the viewer chose none", () => {
    expect(windowsParam({}, false)).toBeUndefined();
  });

  it("names each choice by column in a stable order", () => {
    expect(windowsParam({ verified: "30d", "not-code": "all" }, false)).toBe("not-code:all,verified:30d");
    expect(windowsParam({ "not-code": "all", verified: "30d" }, false)).toBe("not-code:all,verified:30d");
  });

  it("turns every window off in replay whatever the viewer chose", () => {
    expect(windowsParam({ verified: "30d" }, true)).toBe("all");
    expect(windowsParam({}, true)).toBe("all");
  });
});

describe("window words", () => {
  it("says a window in hours, days or all time", () => {
    expect(windowLabel("24h")).toBe("24 hours");
    expect(windowLabel("7d")).toBe("7 days");
    expect(windowLabel("1d")).toBe("1 day");
    expect(windowLabel("all")).toBe("all time");
    expect(windowReach("7d")).toBe("the last 7 days");
    expect(windowReach("all")).toBe("all time");
  });

  it("says how many older items a window hides", () => {
    expect(hiddenText(30, "7d")).toBe("30 older hidden");
    expect(hiddenText(1, "7d")).toBe("1 older hidden");
    expect(hiddenText(0, "7d")).toBe("nothing older");
    expect(hiddenText(0, "all")).toBe("all shown");
  });

  it("names the window only on an empty column whose window hides something", () => {
    expect(emptyText(undefined)).toBe("None");
    expect(emptyText({ window: "7d", hidden: 0 })).toBe("None");
    expect(emptyText({ window: "7d", hidden: 3 })).toBe("None in the last 7 days");
  });
});

describe("cards per column", () => {
  it("offers 5, 10 and 20 with the definition's own value", () => {
    expect(cardChoices(10)).toEqual([5, 10, 20]);
    expect(cardChoices(6)).toEqual([5, 6, 10, 20]);
  });

  it("takes the viewer's choice, then the definition's, then 10", () => {
    const six = { ...orders, board: { cards: 6 } };
    expect(cardsPerColumn(six, 20)).toBe(20);
    expect(cardsPerColumn(six)).toBe(6);
    expect(cardsPerColumn(orders)).toBe(10);
  });
});

describe("columnHeight", () => {
  it("fits the cards, their rung rows and a window footer", () => {
    expect(columnHeight(10, false, false)).toBeCloseTo(62.8955, 4);
    expect(columnHeight(10, true, true)).toBeCloseTo(76.9785, 4);
    expect(columnHeight(1, false, false)).toBeCloseTo(COLUMN_CHROME_REM + CARD_REM, 10);
  });

  it("grows with every card", () => {
    const heights = Array.from({ length: 50 }, (_, i) => columnHeight(i + 1, true, false));
    expect(heights).toEqual([...heights].sort((a, b) => a - b));
    expect(new Set(heights).size).toBe(heights.length);
  });
});

describe("hasWindows", () => {
  it("sees a window on any lane or exit", () => {
    expect(hasWindows(orders)).toBe(true);
    const unwindowed = {
      ...orders,
      lanes: orders.lanes.map(({ id, label }) => ({ id, label })),
      exits: (orders.exits ?? []).map(({ id, label }) => ({ id, label })),
    };
    expect(hasWindows(unwindowed)).toBe(false);
  });
});
