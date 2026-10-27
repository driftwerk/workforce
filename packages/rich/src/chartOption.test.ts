import { describe, expect, it } from "vitest";

import { buildOption, chartAriaLabel } from "./chartOption";
import { CATEGORICAL, SEQUENTIAL, type ChartInk } from "./palette";
import type { ChartSpec } from "./spec";

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

const build = (spec: Partial<ChartSpec> & Pick<ChartSpec, "kind" | "series">) =>
  buildOption(spec as ChartSpec, ink, false) as Record<string, any>;

describe("buildOption", () => {
  it("uses the host's categorical palette and axis tooltips for bar/line", () => {
    const opt = build({ kind: "bar", labels: ["a"], series: [{ data: [1] }] });
    expect(opt.color).toEqual([...ink.series]);
    expect(opt.tooltip.trigger).toBe("axis");
    expect(opt.tooltip.confine).toBe(true);
  });

  it("hides the legend for a single non-pie series and shows it otherwise", () => {
    expect(build({ kind: "line", series: [{ data: [1] }] }).legend.show).toBe(false);
    expect(
      build({ kind: "line", series: [{ data: [1] }, { data: [2] }] }).legend.top,
    ).toBe(0);
    expect(
      build({ kind: "pie", labels: ["a"], series: [{ data: [1] }] }).legend.top,
    ).toBe(0);
  });

  it("renders pie slices with surface gaps, percent labels, and item tooltips", () => {
    const opt = build({
      kind: "pie",
      labels: ["Open", "Closed"],
      series: [{ data: [1, 3] }],
    });
    expect(opt.tooltip.trigger).toBe("item");
    const pie = opt.series[0];
    expect(pie.type).toBe("pie");
    expect(pie.data).toEqual([
      { name: "Open", value: 1 },
      { name: "Closed", value: 3 },
    ]);
    expect(pie.itemStyle.borderColor).toBe(ink.surface);
    expect(pie.label.formatter).toContain("{d}%");
  });

  it("folds pie slices beyond the palette size into Other", () => {
    const labels = Array.from({ length: 11 }, (_, i) => `s${i}`);
    const data = labels.map((_, i) => i + 1);
    const pie = build({ kind: "pie", labels, series: [{ data }] }).series[0];
    expect(pie.data).toHaveLength(8);
    const other = pie.data[7];
    expect(other.name).toBe("Other");
    expect(other.value).toBe(8 + 9 + 10 + 11);
  });

  it("maps stacked and horizontal bars with the measure staying on y_label", () => {
    const opt = build({
      kind: "bar",
      labels: ["a"],
      series: [{ data: [1] }, { data: [2] }],
      stacked: true,
      horizontal: true,
      x_label: "Plan",
      y_label: "Signups",
    });
    expect(opt.series[0].stack).toBe("total");
    expect(opt.series[0].itemStyle.borderColor).toBe(ink.surface);
    expect(opt.xAxis.type).toBe("value");
    expect(opt.xAxis.name).toBe("Signups");
    expect(opt.yAxis.type).toBe("category");
    expect(opt.yAxis.name).toBe("Plan");
  });

  it("gives area charts an areaStyle and line charts a crosshair pointer", () => {
    const area = build({ kind: "area", series: [{ data: [1, 2] }] });
    expect(area.series[0].type).toBe("line");
    expect(area.series[0].areaStyle).toBeTruthy();
    expect(area.tooltip.axisPointer.type).toBe("cross");
    const bar = build({ kind: "bar", series: [{ data: [1] }] });
    expect(bar.tooltip.axisPointer).toBeUndefined();
  });

  it("adds inside zoom only past the threshold", () => {
    const small = build({ kind: "line", series: [{ data: [1, 2, 3] }] });
    expect(small.dataZoom).toBeUndefined();
    const big = build({
      kind: "line",
      series: [{ data: Array.from({ length: 60 }, (_, i) => i) }],
    });
    expect(big.dataZoom).toEqual([{ type: "inside" }]);
  });

  it("defaults missing labels to 1..n and formats tooltip values with the unit", () => {
    const opt = build({ kind: "bar", series: [{ data: [5, 6] }], unit: "£" });
    expect(opt.xAxis.data).toEqual(["1", "2"]);
    expect(opt.tooltip.valueFormatter(1234)).toBe("£1,234");
    expect(opt.tooltip.valueFormatter(523797.33)).toBe("£523,797.33");
    expect(opt.tooltip.valueFormatter(1234.5678)).toBe("£1,234.57");
    expect(opt.tooltip.valueFormatter(null)).toBe("");
  });

  it("formats measure-axis ticks the same way as the tooltip", () => {
    const opt = build({ kind: "bar", series: [{ data: [5, 6] }], unit: "£" });
    expect(opt.yAxis.axisLabel.formatter(1500000)).toBe("£1,500,000");
    const plain = build({ kind: "line", series: [{ data: [5, 6] }] });
    expect(plain.yAxis.axisLabel.formatter(2500.5)).toBe("2,500.5");
    const horizontal = build({ kind: "bar", series: [{ data: [5] }], horizontal: true, unit: "£" });
    expect(horizontal.xAxis.axisLabel.formatter(1000)).toBe("£1,000");
    expect(horizontal.yAxis.axisLabel.formatter).toBeUndefined();
  });

  it("leaves a scatter's year-like x values ungrouped while formatting y", () => {
    const opt = build({
      kind: "scatter",
      series: [{ data: [[2024, 1200000], [2025, 1350000]] as [number, number][] }],
      unit: "£",
    });
    expect(opt.xAxis.axisLabel.formatter(2025)).toBe("2025");
    expect(opt.yAxis.axisLabel.formatter(1350000)).toBe("£1,350,000");
    expect(opt.tooltip.valueFormatter([2025, 1350000])).toBe("2025, £1,350,000");
    const plain = build({ kind: "scatter", series: [{ data: [[1500, 2], [3000, 4]] as [number, number][] }] });
    expect(plain.xAxis.axisLabel.formatter(3000)).toBe("3,000");
  });

  it("disables animation under reduced motion", () => {
    const opt = buildOption(
      { kind: "bar", series: [{ data: [1] }] } as ChartSpec,
      ink,
      true,
    ) as Record<string, any>;
    expect(opt.animation).toBe(false);
  });

  it("maps scatter to value axes with pair data", () => {
    const opt = build({
      kind: "scatter",
      series: [{ data: [[1, 2], [3, 4]] as [number, number][] }],
      x_label: "Age",
      y_label: "Spend",
    });
    expect(opt.series[0].type).toBe("scatter");
    expect(opt.xAxis.type).toBe("value");
    expect(opt.xAxis.name).toBe("Age");
    expect(opt.yAxis.name).toBe("Spend");
  });
});

describe("chartAriaLabel", () => {
  it("names the kind and title", () => {
    expect(chartAriaLabel({ kind: "pie", series: [] } as unknown as ChartSpec)).toBe(
      "pie chart",
    );
    expect(
      chartAriaLabel({ kind: "area", title: "Growth", series: [] } as unknown as ChartSpec),
    ).toBe("area chart: Growth");
  });
});
