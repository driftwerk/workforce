// Pure mapping from a validated ChartSpec to an ECharts option object. Kept
// free of echarts imports so every branch is unit-testable; only validated
// scalars reach the option — no payload-supplied formatters or HTML.

import type { ChartSpec } from "./spec";
import type { ChartInk } from "./palette";
import { formatNumber, looksLikeYears, type NumberStyle } from "./format";

/** Slices beyond this fold into "Other" — the palette has 8 identity slots. */
const MAX_PIE_SLICES = 8;
/** Category count beyond which line/area/bar charts get inside-zoom. */
const ZOOM_THRESHOLD = 50;

type Option = Record<string, unknown>;

const axisLabel = (ink: ChartInk) => ({ color: ink.muted, fontFamily: ink.font });
const axisLine = (ink: ChartInk) => ({ lineStyle: { color: ink.axis } });

function categoryAxis(ink: ChartInk, labels: string[]): Option {
  return {
    type: "category",
    data: labels,
    axisLabel: axisLabel(ink),
    axisLine: axisLine(ink),
    axisTick: { show: false },
  };
}

/** A measure axis: ticks carry the same separators and unit as the tooltip, so
 *  a reader never sees `523797` on the axis and `£523,797` on hover. */
function valueAxis(name: string | undefined, ink: ChartInk, style: NumberStyle): Option {
  return {
    type: "value",
    name,
    nameTextStyle: { color: ink.muted, fontFamily: ink.font },
    axisLabel: { ...axisLabel(ink), formatter: (v: number) => formatNumber(v, style) },
    splitLine: { lineStyle: { color: ink.grid } },
  };
}

/** A scatter's x values are the one chart input that may be years. */
function scatterXStyle(spec: ChartSpec): NumberStyle {
  const xs = spec.series.flatMap((s) => (s.data as [number, number][]).map((p) => p[0]));
  return looksLikeYears(xs) ? { format: "id" } : {};
}

function pieData(spec: ChartSpec): { name: string; value: number }[] {
  const labels = spec.labels ?? [];
  const slices = (spec.series[0].data as number[]).map((value, i) => ({
    name: labels[i] ?? `Slice ${i + 1}`,
    value,
  }));
  if (slices.length <= MAX_PIE_SLICES) return slices;
  const kept = slices.slice(0, MAX_PIE_SLICES - 1);
  const other = slices
    .slice(MAX_PIE_SLICES - 1)
    .reduce((sum, s) => sum + s.value, 0);
  return [...kept, { name: "Other", value: other }];
}

export function buildOption(
  spec: ChartSpec,
  ink: ChartInk,
  reducedMotion: boolean,
): Option {
  const seriesCount = spec.series.length;
  const named = (i: number) => spec.series[i].name ?? `Series ${i + 1}`;
  const showLegend = spec.kind === "pie" || seriesCount > 1;
  const measure: NumberStyle = { unit: spec.unit };
  const xStyle = spec.kind === "scatter" ? scatterXStyle(spec) : measure;
  // Scatter points reach the tooltip as `[x, y]`; everything else as a number.
  const valueFormatter = (value: unknown): string => {
    if (typeof value === "number") return formatNumber(value, measure);
    if (Array.isArray(value) && value.length === 2 && value.every((v) => typeof v === "number")) {
      return `${formatNumber(value[0], xStyle)}, ${formatNumber(value[1], measure)}`;
    }
    return String(value ?? "");
  };

  const base: Option = {
    color: [...ink.series],
    animation: !reducedMotion,
    animationDuration: 300,
    textStyle: { color: ink.text, fontFamily: ink.font },
    tooltip: {
      confine: true,
      trigger: spec.kind === "pie" || spec.kind === "scatter" ? "item" : "axis",
      valueFormatter,
      ...(spec.kind === "line" || spec.kind === "area"
        ? { axisPointer: { type: "cross", label: { show: false } } }
        : {}),
    },
    legend: showLegend
      ? {
          top: 0,
          type: "scroll",
          icon: "circle",
          textStyle: { color: ink.text, fontFamily: ink.font },
        }
      : { show: false },
  };

  if (spec.kind === "pie") {
    return {
      ...base,
      series: [
        {
          type: "pie",
          radius: "68%",
          center: ["50%", "56%"],
          data: pieData(spec),
          label: { formatter: "{b}  {d}%", color: ink.text, fontFamily: ink.font },
          itemStyle: { borderColor: ink.surface, borderWidth: 2 },
          emphasis: { scaleSize: 4 },
        },
      ],
    };
  }

  const labels =
    spec.kind === "scatter"
      ? []
      : (spec.labels ??
        Array.from(
          { length: Math.max(...spec.series.map((s) => s.data.length)) },
          (_, i) => String(i + 1),
        ));
  const zoomed = spec.kind !== "scatter" && labels.length > ZOOM_THRESHOLD;

  const series = spec.series.map((s, i) => {
    switch (spec.kind) {
      case "bar":
        return {
          type: "bar",
          name: named(i),
          data: s.data,
          stack: spec.stacked ? "total" : undefined,
          itemStyle: spec.stacked
            ? { borderColor: ink.surface, borderWidth: 1 }
            : { borderRadius: spec.horizontal ? [0, 4, 4, 0] : [4, 4, 0, 0] },
        };
      case "scatter":
        return { type: "scatter", name: named(i), data: s.data, symbolSize: 8 };
      default:
        return {
          type: "line",
          name: named(i),
          data: s.data,
          stack: spec.stacked ? "total" : undefined,
          lineStyle: { width: 2 },
          symbol: "circle",
          symbolSize: 6,
          showSymbol: s.data.length <= 30,
          ...(spec.kind === "area" ? { areaStyle: { opacity: 0.18 } } : {}),
        };
    }
  });

  const catAxis = categoryAxis(ink, labels);
  const xValue = valueAxis(spec.x_label, ink, xStyle);
  const yValue = valueAxis(spec.y_label, ink, measure);
  return {
    ...base,
    grid: { left: 8, right: 16, top: showLegend ? 36 : 16, bottom: 8, containLabel: true },
    // The measure keeps `y_label` and the categories keep `x_label` even when a
    // horizontal bar swaps which screen axis they occupy.
    xAxis:
      spec.kind === "scatter" ? xValue : spec.horizontal ? yValue : { ...catAxis, name: spec.x_label },
    yAxis:
      spec.kind === "scatter" ? yValue : spec.horizontal ? { ...catAxis, name: spec.x_label } : yValue,
    ...(zoomed ? { dataZoom: [{ type: "inside" }] } : {}),
    series,
  };
}

/** The accessible one-line description of the chart. */
export function chartAriaLabel(spec: ChartSpec): string {
  const kind = spec.kind === "area" ? "area chart" : `${spec.kind} chart`;
  return spec.title ? `${kind}: ${spec.title}` : kind;
}
