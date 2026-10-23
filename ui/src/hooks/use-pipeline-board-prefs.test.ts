import { describe, expect, it } from "vitest";
import { readBoardPrefs } from "./use-pipeline-board-prefs";

describe("readBoardPrefs", () => {
  it("starts from the definition's values without storage or with anything that is not an object", () => {
    for (const raw of [null, "not json", "[1,2]", "null", "7"]) {
      expect(readBoardPrefs(raw), String(raw)).toEqual({ windows: {} });
    }
  });

  it("keeps a whole number of cards from 1 to 50", () => {
    for (const cards of [1, 10, 50]) {
      expect(readBoardPrefs(JSON.stringify({ cards }))).toEqual({ cards, windows: {} });
    }
    for (const cards of [0, 51, "10", 10.5, null]) {
      expect(readBoardPrefs(JSON.stringify({ cards })), String(cards)).toEqual({ windows: {} });
    }
  });

  it("drops a window the hub would refuse, keeps the rest and ignores unknown keys", () => {
    const stored = {
      windows: { verified: "30d", "not-code": "2h", duplicate: 7, superseded: "week", regression: "all" },
      theme: "dark",
    };
    expect(readBoardPrefs(JSON.stringify(stored))).toEqual({ windows: { verified: "30d", regression: "all" } });
    expect(readBoardPrefs(JSON.stringify({ cards: 20, windows: ["7d"] }))).toEqual({ cards: 20, windows: {} });
  });
});
