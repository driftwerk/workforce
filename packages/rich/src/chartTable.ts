// A chart's data as a table: what the reader sees when they ask for the
// numbers behind the picture. Pure, from the validated spec alone.

import type { ChartSpec, InlineTableSpec, TableColumn } from "./spec";

const seriesName = (spec: ChartSpec, i: number) => spec.series[i].name ?? `Series ${i + 1}`;

const measure = (spec: ChartSpec, key: string, label: string): TableColumn => ({
  key,
  label,
  align: "right",
  format: "number",
  ...(spec.unit ? { unit: spec.unit } : {}),
});

/** The rows a chart was drawn from: one per category with a column per
 *  series, or for a scatter one per point (with its series when there are
 *  several). */
export function chartTable(spec: ChartSpec): InlineTableSpec {
  if (spec.kind === "scatter") {
    const named = spec.series.length > 1;
    const columns: TableColumn[] = [
      ...(named ? [{ key: "series", label: "Series" }] : []),
      { key: "x", label: spec.x_label ?? "x", align: "right", format: "number" },
      measure(spec, "y", spec.y_label ?? "y"),
    ];
    const rows = spec.series.flatMap((s, i) =>
      (s.data as [number, number][]).map(([x, y]) => (named ? [seriesName(spec, i), x, y] : [x, y])),
    );
    return { mode: "inline", columns, rows };
  }
  const length = Math.max(...spec.series.map((s) => s.data.length));
  const labels = spec.labels ?? Array.from({ length }, (_, i) => String(i + 1));
  const single = spec.series.length === 1;
  const columns: TableColumn[] = [
    { key: "category", label: spec.x_label ?? (spec.kind === "pie" ? "Slice" : "Category") },
    ...spec.series.map((_, i) =>
      measure(spec, `s${i}`, single && !spec.series[0].name ? (spec.y_label ?? "Value") : seriesName(spec, i)),
    ),
  ];
  const rows = labels.map((label, row) => [
    label,
    ...spec.series.map((s) => (s.data as number[])[row] ?? null),
  ]);
  return { mode: "inline", columns, rows };
}
