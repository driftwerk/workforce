import { describe, expect, it } from "vitest";
import { LABEL_MAX, type PipelineDefinition, isAnnotation, labelText, places, positionFor, ruleLabel } from "./definition";
import { annotated } from "./testdata/annotated";

describe("labelText", () => {
  it("makes one trimmed line of the text", () => {
    expect(labelText("  merged by octocat,\n waiting for the deploy  ")).toBe("merged by octocat, waiting for the deploy");
    expect(labelText("tab\tand\u0007bell")).toBe("tab and bell");
  });

  it("keeps what the hub keeps: a byte-order mark is neither space nor control", () => {
    expect(labelText("\uFEFFnote\uFEFF")).toBe("\uFEFFnote\uFEFF");
  });

  it("drops angle brackets, so no Slack mention or link survives", () => {
    expect(labelText("<!channel> ping <@U123>")).toBe("!channel ping @U123");
  });

  it("gives nothing for blank text or a value that is not text", () => {
    for (const value of ["", "   ", 42, null, undefined, { note: "x" }]) {
      expect(labelText(value), String(value)).toBeNull();
    }
  });

  it(`keeps ${LABEL_MAX} code points and cuts longer text with an ellipsis`, () => {
    expect(labelText("x".repeat(120))).toBe("x".repeat(120));
    expect(labelText("x".repeat(121))).toBe(`${"x".repeat(119)}…`);
    expect(labelText("é".repeat(130))).toBe(`${"é".repeat(119)}…`);
    const words = "word ".repeat(30);
    expect(labelText(words)).toBe(`${Array.from(words).slice(0, 119).join("").trimEnd()}…`);
  });
});

describe("ruleLabel", () => {
  const def: PipelineDefinition = {
    ...annotated,
    events: {
      ...annotated.events,
      "merge/merged": [{ column: "reviewed", tone: "active", label: "merged", label_field: "note" }],
      "merge/noted": [{ column: "reviewed", label_field: "note" }],
    },
  };
  const rule = (key: string) => def.events[key]?.[0] ?? {};

  it("prefers the label field's text, then the label, then the column's label", () => {
    expect(ruleLabel(def, rule("merge/merged"), { note: "merged by octocat" }, "merge", "merged")).toBe("merged by octocat");
    for (const detail of [{ note: "  " }, { note: 7 }, {}, null]) {
      expect(ruleLabel(def, rule("merge/merged"), detail, "merge", "merged")).toBe("merged");
    }
    expect(ruleLabel(def, rule("merge/noted"), {}, "merge", "noted")).toBe("Reviewed");
  });

  it("names an annotation without a label by its event", () => {
    const judged = rule("review/overlap-judged");
    expect(ruleLabel(def, judged, { headline: "#1702 covers #1698" }, "review", "overlap-judged")).toBe("#1702 covers #1698");
    expect(ruleLabel(def, judged, {}, "review", "overlap-judged")).toBe("review/overlap-judged");
    expect(ruleLabel(def, rule("watch/resolved"), {}, "watch", "resolved")).toBe("resolved");
  });

  it("places with the label field's text", () => {
    expect(positionFor(def, "merge", "merged", { note: "merged by octocat" })?.label).toBe("merged by octocat");
  });
});

describe("annotation rules", () => {
  it("never place an item", () => {
    expect(positionFor(annotated, "review", "overlap-judged", { headline: "x" })).toBeNull();
    expect(isAnnotation({ annotation: true })).toBe(true);
    expect(places({ annotation: true, column: "reviewed" })).toBe(false);
    expect(places({})).toBe(false);
    expect(places({ column: "reviewed" })).toBe(true);
  });
});
