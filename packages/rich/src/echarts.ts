// The one place echarts is imported: the tree-shakeable core plus exactly the
// chart types and components wf-chart renders. Importing the package root (or
// echarts-for-react) would ship the full library — never do that here.

import * as echarts from "echarts/core";
import { BarChart, LineChart, MapChart, PieChart, ScatterChart } from "echarts/charts";
import {
  DataZoomComponent,
  GridComponent,
  LegendComponent,
  TooltipComponent,
  VisualMapComponent,
} from "echarts/components";
import { SVGRenderer } from "echarts/renderers";

echarts.use([
  BarChart,
  LineChart,
  MapChart,
  PieChart,
  ScatterChart,
  GridComponent,
  TooltipComponent,
  LegendComponent,
  DataZoomComponent,
  VisualMapComponent,
  SVGRenderer,
]);

export type EChart = ReturnType<typeof echarts.init>;

export function initChart(el: HTMLElement): EChart {
  return echarts.init(el, undefined, { renderer: "svg" });
}

/** Register a geography's GeoJSON under `id` so a `map` series can reference it. */
export function registerGeoMap(id: string, geo: unknown): void {
  echarts.registerMap(id, geo as Parameters<typeof echarts.registerMap>[1]);
}
