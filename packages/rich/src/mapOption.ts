// Pure mapping from a validated MapSpec + its geography to an ECharts option.
// Free of echarts imports so every branch is unit-testable. Region names are
// resolved against the geography's own feature names (alias, then exact, then
// case-insensitive); anything unresolved is surfaced, never dropped silently.

import type { ChartInk } from "./palette";
import type { GeoFeatureCollection } from "./geo/registry";
import type { MapSpec } from "./spec";
import { formatNumber } from "./format";

type Option = Record<string, unknown>;
type BoundingCoords = [[number, number], [number, number]];

/** Top regions named for the accessible summary. */
const ARIA_TOP = 3;
/** The view frames the fewest top-value regions holding this share of the total,
 *  so a few far-flung low-value outliers can't shrink the dense area to a speck
 *  (nearly all leads sit in London — frame London, not the whole of Britain). */
const FRAME_COVERAGE = 0.9;
/** Breathing room around the framed regions, as a fraction of their extent. */
const FRAME_PAD = 0.12;

/** A tooltip function-formatter's return is inserted as raw HTML, so every
 *  agent-supplied value interpolated into it must be escaped. */
const escapeHtml = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

interface Resolved {
  data: { name: string; value: number }[];
  unplaced: string[];
}

function resolveRegions(
  spec: MapSpec,
  names: string[],
  aliases: Record<string, string>,
): Resolved {
  const canonical = new Map(names.map((n) => [n.toLowerCase(), n]));
  const totals = new Map<string, number>();
  const order: string[] = [];
  const unplaced: string[] = [];
  for (const region of spec.regions) {
    const key = region.name.toLowerCase();
    const aliased = aliases[key]?.toLowerCase();
    const match = (aliased && canonical.get(aliased)) || canonical.get(key);
    if (!match) {
      unplaced.push(region.name);
      continue;
    }
    if (!totals.has(match)) order.push(match);
    totals.set(match, (totals.get(match) ?? 0) + region.value);
  }
  return { data: order.map((name) => ({ name, value: totals.get(name)! })), unplaced };
}

/** Grow a [west, south, east, north] box to include every coordinate pair. */
function extendBox(box: number[], coords: unknown): void {
  if (!Array.isArray(coords)) return;
  if (typeof coords[0] === "number" && typeof coords[1] === "number") {
    const [lng, lat] = coords as [number, number];
    if (lng < box[0]) box[0] = lng;
    if (lat < box[1]) box[1] = lat;
    if (lng > box[2]) box[2] = lng;
    if (lat > box[3]) box[3] = lat;
    return;
  }
  for (const c of coords) extendBox(box, c);
}

interface MapView {
  center: [number, number];
  zoom: number;
  boundingCoords: BoundingCoords;
  scaleMax: number;
}

/** Bounding [west, south, east, north] over the features `keep` selects, or null. */
function boxOf(geo: GeoFeatureCollection, keep: (name: string) => boolean): number[] | null {
  const box = [Infinity, Infinity, -Infinity, -Infinity];
  for (const f of geo.features) if (keep(f.properties.name)) extendBox(box, f.geometry.coordinates);
  return box[0] > box[2] ? null : box;
}

const padBox = (b: number[], f: number): BoundingCoords => {
  const px = (b[2] - b[0]) * f || f;
  const py = (b[3] - b[1]) * f || f;
  return [
    [b[0] - px, b[3] + py],
    [b[2] + px, b[1] - py],
  ];
};

/** Open zoomed into the data-dense area (the top-value regions covering
 *  FRAME_COVERAGE of the total) while leaving the whole map pannable and
 *  zoomable — boundingCoords spans the full geography, center + zoom start over
 *  the dense area, and scaleLimit lets the view zoom back out to the whole map.
 *  Null when the data already covers everything (no reframing needed). */
function frameView(
  geo: GeoFeatureCollection,
  data: { name: string; value: number }[],
): MapView | null {
  if (data.length === 0) return null;
  const total = data.reduce((sum, d) => sum + Math.max(0, d.value), 0);
  const frame = new Set<string>();
  let acc = 0;
  for (const d of [...data].sort((a, b) => b.value - a.value)) {
    frame.add(d.name);
    acc += Math.max(0, d.value);
    if (total > 0 && acc >= total * FRAME_COVERAGE) break;
  }
  if (frame.size >= geo.features.length) return null;
  const dense = boxOf(geo, (n) => frame.has(n));
  const full = boxOf(geo, () => true);
  if (!dense || !full) return null;
  const boundingCoords = padBox(full, FRAME_PAD);
  const view = padBox(dense, FRAME_PAD);
  const spanW = boundingCoords[1][0] - boundingCoords[0][0];
  const spanH = boundingCoords[0][1] - boundingCoords[1][1];
  const zoom = Math.max(1, Math.min(spanW / (view[1][0] - view[0][0]), spanH / (view[0][1] - view[1][1])));
  return {
    center: [(dense[0] + dense[2]) / 2, (dense[1] + dense[3]) / 2],
    zoom,
    boundingCoords,
    scaleMax: Math.max(12, Math.ceil(zoom) + 3),
  };
}

function ariaLabel(spec: MapSpec, data: { name: string; value: number }[]): string {
  const head = spec.title ? `Choropleth: ${spec.title}` : "Choropleth map";
  if (data.length === 0) return head;
  const top = [...data]
    .sort((a, b) => b.value - a.value)
    .slice(0, ARIA_TOP)
    .map((d) => `${d.name} ${formatNumber(d.value, { unit: spec.unit })}`)
    .join(", ");
  return `${head}; highest ${top}`;
}

/** Everything in the option that carries colour. Built once for both paths so a
 *  theme change can re-apply it alone, leaving the reader's pan and zoom. */
function paint(ink: ChartInk) {
  return {
    textStyle: { color: ink.text, fontFamily: ink.font },
    legendText: { color: ink.muted, fontFamily: ink.font },
    ramp: { color: [...ink.sequential] },
    itemStyle: { areaColor: ink.grid, borderColor: ink.muted, borderWidth: 0.6 },
    emphasis: { label: { show: false }, itemStyle: { borderColor: ink.text, borderWidth: 1.2 } },
  };
}

/** The colour-only option a drawn map merges in when the theme changes. */
export function mapPaintOption(ink: ChartInk): Option {
  const colours = paint(ink);
  return {
    textStyle: colours.textStyle,
    visualMap: { textStyle: colours.legendText, inRange: colours.ramp },
    series: [{ itemStyle: colours.itemStyle, emphasis: colours.emphasis }],
  };
}

export function buildMapOption(
  spec: MapSpec,
  geo: GeoFeatureCollection,
  aliases: Record<string, string>,
  ink: ChartInk,
  reducedMotion: boolean,
): { option: Option; unplaced: string[]; ariaLabel: string } {
  const { data, unplaced } = resolveRegions(
    spec,
    geo.features.map((f) => f.properties.name),
    aliases,
  );
  const values = data.map((d) => d.value);
  const min = spec.min ?? Math.min(0, ...values);
  const ceil = spec.max ?? (values.length ? Math.max(...values) : 1);
  const max = ceil > min ? ceil : min + 1;
  const format = (v: number) => formatNumber(v, { unit: spec.unit });
  const view = frameView(geo, data);
  const colours = paint(ink);

  const option: Option = {
    animation: !reducedMotion,
    animationDuration: 300,
    textStyle: colours.textStyle,
    tooltip: {
      trigger: "item",
      confine: true,
      formatter: (p: { name?: string; value?: unknown }) => {
        const shown = typeof p.value === "number" && Number.isFinite(p.value) ? format(p.value) : "—";
        const label = spec.value_label ? ` ${spec.value_label}` : "";
        return escapeHtml(`${p.name ?? ""}: ${shown}${label}`);
      },
    },
    visualMap: {
      type: "continuous",
      min,
      max,
      calculable: false,
      left: "left",
      bottom: "3%",
      itemWidth: 10,
      itemHeight: 92,
      text: [format(max), format(min)],
      textStyle: colours.legendText,
      inRange: colours.ramp,
    },
    series: [
      {
        type: "map",
        map: spec.geo,
        nameProperty: "name",
        roam: true,
        scaleLimit: { min: 1, max: view?.scaleMax ?? 12 },
        data,
        itemStyle: colours.itemStyle,
        emphasis: colours.emphasis,
        select: { disabled: true },
        ...(view
          ? { center: view.center, zoom: view.zoom, boundingCoords: view.boundingCoords }
          : {}),
      },
    ],
  };
  return { option, unplaced, ariaLabel: ariaLabel(spec, data) };
}
