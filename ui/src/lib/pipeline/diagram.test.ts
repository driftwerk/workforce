import { describe, expect, it } from "vitest";
import { layout } from "./diagram";
import { annotated } from "./testdata/annotated";
import { orders } from "./testdata/example";

describe("layout", () => {
  it("draws no edge, box or orphan stage for an annotation rule", () => {
    const plain = layout(orders);
    const noted = layout(annotated);
    expect(noted?.edges.map((e) => e.key)).toEqual(plain?.edges.map((e) => e.key));
    expect(noted?.unknownColumns).toEqual([]);
    expect(noted?.orphanStages).toEqual(plain?.orphanStages);
    expect(noted?.orphanStages).not.toContain("watch");
  });
});
