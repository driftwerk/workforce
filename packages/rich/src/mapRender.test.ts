// @vitest-environment node
//
// Integration cover for what the unit suite is structurally blind to: the real
// boundary data actually renders, real region names resolve, and echarts honors
// the fit-to-data viewport. Loads the real geojson (the one place a test may) and
// runs in node to drive echarts' SSR renderer.

import * as echarts from "echarts/core";
import { MapChart } from "echarts/charts";
import { TooltipComponent, VisualMapComponent } from "echarts/components";
import { SVGRenderer } from "echarts/renderers";
import { describe, expect, it } from "vitest";

import ukGeoJson from "./geo/uk-postcode-areas.json";
import worldGeoJson from "./geo/world-countries.json";
import { buildMapOption } from "./mapOption";
import { CATEGORICAL, SEQUENTIAL, type ChartInk } from "./palette";
import { GEOGRAPHIES, type GeoFeatureCollection } from "./geo/registry";
import type { MapSpec } from "./spec";

echarts.use([MapChart, VisualMapComponent, TooltipComponent, SVGRenderer]);

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

const ukGeo = ukGeoJson as unknown as GeoFeatureCollection;
const worldGeo = worldGeoJson as unknown as GeoFeatureCollection;

const render = (option: Record<string, unknown>) => {
  const chart = echarts.init(null, null, { renderer: "svg", ssr: true, width: 800, height: 600 });
  chart.setOption(option);
  const svg = chart.renderToSVGString();
  chart.dispose();
  return svg;
};

describe("wf-map real-geography render", () => {
  it("resolves real UK codes, draws, and frames London despite far outliers", () => {
    echarts.registerMap("uk-postcode-areas", ukGeo as never);
    // London holds nearly all the leads; EH (Edinburgh ~56°N) and M (Manchester
    // ~53.5°N) are tiny outliers that must NOT stretch the frame off London.
    const spec: MapSpec = {
      kind: "choropleth",
      geo: "uk-postcode-areas",
      regions: [
        { name: "W", value: 489 },
        { name: "SW", value: 451 },
        { name: "SE", value: 416 },
        { name: "N", value: 351 },
        { name: "NW", value: 308 },
        { name: "E", value: 303 },
        { name: "EC", value: 92 },
        { name: "WC", value: 44 },
        { name: "EH", value: 1 },
        { name: "M", value: 1 },
      ],
    };
    const { option, unplaced } = buildMapOption(spec, ukGeo, {}, ink, true);
    expect(unplaced).toEqual([]);
    const s = (option.series as Record<string, unknown>[])[0] as Record<string, any>;
    // Opens centered + zoomed on London (~51.5°N), not Manchester/Edinburgh.
    expect(s.center[1]).toBeGreaterThan(51);
    expect(s.center[1]).toBeLessThan(52);
    expect(s.zoom).toBeGreaterThan(5);
    // boundingCoords still spans the whole country (past Scotland ~56°N), so the
    // view can zoom/pan back out to the outliers.
    expect(s.boundingCoords[0][1]).toBeGreaterThan(55);
    expect(s.scaleLimit.min).toBe(1);
    expect((render(option).match(/<path/g) ?? []).length).toBeGreaterThan(100);
  });

  it("keeps every world-countries alias target a real feature name", () => {
    const names = new Set(worldGeo.features.map((f) => f.properties.name));
    for (const target of Object.values(GEOGRAPHIES["world-countries"].aliases ?? {})) {
      expect(names.has(target)).toBe(true);
    }
  });
});
