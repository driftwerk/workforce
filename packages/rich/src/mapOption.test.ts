import { describe, expect, it } from "vitest";

import { buildMapOption, mapPaintOption } from "./mapOption";
import { CATEGORICAL, SEQUENTIAL, type ChartInk } from "./palette";
import type { GeoFeatureCollection } from "./geo/registry";
import type { MapSpec } from "./spec";

const ink: ChartInk = {
  text: "#111",
  muted: "#888",
  grid: "#eee",
  axis: "#ccc",
  surface: "#fff",
  font: "sans-serif",
  series: CATEGORICAL,
  sequential: SEQUENTIAL,
};

/** Four unit squares at known corners so bounding boxes are exact. */
const square = (name: string, x: number, y: number): GeoFeatureCollection["features"][number] => ({
  type: "Feature",
  properties: { name },
  geometry: {
    type: "Polygon",
    coordinates: [
      [
        [x, y],
        [x + 1, y],
        [x + 1, y + 1],
        [x, y + 1],
        [x, y],
      ],
    ],
  },
});

const geo: GeoFeatureCollection = {
  type: "FeatureCollection",
  features: [square("A", 0, 0), square("B", 2, 2), square("C", 4, 4), square("D", 10, 10)],
};

const spec = (regions: MapSpec["regions"], extra: Partial<MapSpec> = {}): MapSpec => ({
  kind: "choropleth",
  geo: "world-countries",
  regions,
  ...extra,
});

const build = (regions: MapSpec["regions"], aliases = {}, extra: Partial<MapSpec> = {}) =>
  buildMapOption(spec(regions, extra), geo, aliases, ink, false);

describe("buildMapOption", () => {
  it("wires the sequential ramp and a neutral no-data fill distinct from the ramp low", () => {
    const { option } = build([{ name: "A", value: 5 }]);
    const opt = option as Record<string, any>;
    expect(opt.visualMap.inRange.color).toEqual([...ink.sequential]);
    expect(opt.series[0].itemStyle.areaColor).toBe(ink.grid);
    expect(opt.series[0].itemStyle.borderColor).toBe(ink.muted);
    expect(opt.series[0].itemStyle.areaColor).not.toBe(SEQUENTIAL[0]);
    expect(opt.series[0].itemStyle.areaColor).not.toBe(opt.series[0].itemStyle.borderColor);
    expect(opt.series[0].type).toBe("map");
    expect(opt.series[0].map).toBe("world-countries");
  });

  it("resolves names by exact, case-insensitive, and alias, listing the rest", () => {
    const { option, unplaced } = build(
      [
        { name: "a", value: 1 },
        { name: "usa", value: 2 },
        { name: "Z", value: 9 },
      ],
      { usa: "B" },
    );
    const data = (option as Record<string, any>).series[0].data as { name: string; value: number }[];
    expect(data).toEqual([
      { name: "A", value: 1 },
      { name: "B", value: 2 },
    ]);
    expect(unplaced).toEqual(["Z"]);
  });

  it("sums duplicate resolved regions", () => {
    const { option } = build([
      { name: "A", value: 3 },
      { name: "a", value: 4 },
    ]);
    const data = (option as Record<string, any>).series[0].data;
    expect(data).toEqual([{ name: "A", value: 7 }]);
  });

  it("opens centered and zoomed on the high-value cluster, whole map still framed", () => {
    const { option } = build([
      { name: "A", value: 100 },
      { name: "D", value: 1 },
    ]);
    const s = (option as Record<string, any>).series[0];
    // Centered + zoomed on A ([0,0]-[1,1]); the D outlier is off the initial view.
    expect(s.center).toEqual([0.5, 0.5]);
    expect(s.zoom).toBeGreaterThan(1);
    // boundingCoords spans the whole geography, so it can pan/zoom out to D.
    expect(s.boundingCoords[1][0]).toBeGreaterThan(10); // east reaches D at 11
    expect(s.scaleLimit.min).toBe(1);
  });

  it("omits the framed view when every feature or no feature has data", () => {
    const all = build([
      { name: "A", value: 1 },
      { name: "B", value: 1 },
      { name: "C", value: 1 },
      { name: "D", value: 1 },
    ]);
    expect((all.option as Record<string, any>).series[0].boundingCoords).toBeUndefined();
    expect((all.option as Record<string, any>).series[0].zoom).toBeUndefined();
    const none = build([{ name: "Z", value: 1 }]);
    expect((none.option as Record<string, any>).series[0].boundingCoords).toBeUndefined();
  });

  it("guards a degenerate scale where the ceiling does not exceed the floor", () => {
    expect((build([{ name: "A", value: 0 }]).option as Record<string, any>).visualMap.max).toBe(1);
    const pinned = build([{ name: "A", value: 5 }], {}, { min: 5, max: 5 });
    expect((pinned.option as Record<string, any>).visualMap.max).toBe(6);
  });

  it("summarizes the top regions for the accessible label", () => {
    const { ariaLabel } = build(
      [
        { name: "A", value: 10 },
        { name: "B", value: 40 },
      ],
      {},
      { title: "Leads by area" },
    );
    expect(ariaLabel).toBe("Choropleth: Leads by area; highest B 40, A 10");
  });

  it("spans every part of a MultiPolygon when framing to data", () => {
    const multi: GeoFeatureCollection = {
      type: "FeatureCollection",
      features: [
        {
          type: "Feature",
          properties: { name: "M" },
          geometry: {
            type: "MultiPolygon",
            coordinates: [
              [
                [
                  [0, 0],
                  [1, 0],
                  [1, 1],
                  [0, 1],
                  [0, 0],
                ],
              ],
              [
                [
                  [20, 20],
                  [21, 20],
                  [21, 21],
                  [20, 21],
                  [20, 20],
                ],
              ],
            ],
          },
        },
        square("Z", 50, 50),
      ],
    };
    const { option } = buildMapOption(
      { kind: "choropleth", geo: "world-countries", regions: [{ name: "M", value: 1 }] },
      multi,
      {},
      ink,
      false,
    );
    const s = (option as Record<string, any>).series[0];
    // The centroid reflects BOTH disjoint parts of M's MultiPolygon (~10.5), not
    // just the near [0,0] part (~0.5).
    expect(s.center[0]).toBeGreaterThan(5);
    expect(s.center[1]).toBeGreaterThan(5);
    // boundingCoords still spans the whole geography (out to Z at 50).
    expect(s.boundingCoords[1][0]).toBeGreaterThan(50);
  });

  it("escapes agent-supplied unit and value_label in the tooltip", () => {
    const { option } = build([{ name: "A", value: 5 }], {}, {
      unit: "<img src=x onerror=alert(1)>",
      value_label: "<b>x</b>",
    });
    const formatter = (option as Record<string, any>).tooltip.formatter as (p: unknown) => string;
    const html = formatter({ name: "A", value: 5 });
    expect(html).not.toContain("<img");
    expect(html).not.toContain("<b>");
    expect(html).toContain("&#60;");
  });
});

describe("mapPaintOption", () => {
  // A theme change merges this alone into the drawn map, so it must carry
  // every colour the full option does — and nothing that moves the view.
  it("repaints exactly what the full option paints", () => {
    const { option } = build([{ name: "A", value: 5 }]);
    const full = option as Record<string, any>;
    const paint = mapPaintOption(ink) as Record<string, any>;
    expect(paint.textStyle).toEqual(full.textStyle);
    expect(paint.visualMap.inRange).toEqual(full.visualMap.inRange);
    expect(paint.visualMap.textStyle).toEqual(full.visualMap.textStyle);
    expect(paint.series[0].itemStyle).toEqual(full.series[0].itemStyle);
    expect(paint.series[0].emphasis).toEqual(full.series[0].emphasis);
    expect(Object.keys(paint.series[0]).sort()).toEqual(["emphasis", "itemStyle"]);
  });
});
