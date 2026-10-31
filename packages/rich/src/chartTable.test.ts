import { describe, expect, it } from "vitest";

import { chartTable } from "./chartTable";
import { validateChart, validateTable, type ChartSpec } from "./spec";

const chart = (value: unknown): ChartSpec => {
  const v = validateChart(value);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
};

describe("chartTable", () => {
  it("is a row per category and a column per series, in the chart's unit", () => {
    const table = chartTable(
      chart({
        kind: "bar",
        unit: "£",
        labels: ["Free", "Pro"],
        series: [
          { name: "March", data: [420, 180] },
          { name: "April", data: [510, 220] },
        ],
      }),
    );
    expect(table.columns.map((c) => c.label)).toEqual(["Category", "March", "April"]);
    expect(table.columns[1]).toMatchObject({ align: "right", format: "number", unit: "£" });
    expect(table.rows).toEqual([
      ["Free", 420, 510],
      ["Pro", 180, 220],
    ]);
  });

  it("names a pie's slices and its one unnamed series by the axis labels it has", () => {
    const table = chartTable(chart({ kind: "pie", labels: ["Open", "Closed"], series: [{ data: [14, 52] }] }));
    expect(table.columns.map((c) => c.label)).toEqual(["Slice", "Value"]);
    expect(table.rows).toEqual([
      ["Open", 14],
      ["Closed", 52],
    ]);
    const labelled = chartTable(
      chart({ kind: "line", x_label: "Month", y_label: "Signups", series: [{ data: [1, 2, 3] }] }),
    );
    expect(labelled.columns.map((c) => c.label)).toEqual(["Month", "Signups"]);
    expect(labelled.rows.map((r) => r[0])).toEqual(["1", "2", "3"]);
  });

  it("numbers unlabelled categories and leaves a short series' missing values empty", () => {
    const table = chartTable(chart({ kind: "line", series: [{ data: [1, 2] }, { name: "short", data: [3] }] }));
    expect(table.rows).toEqual([
      ["1", 1, 3],
      ["2", 2, null],
    ]);
  });

  it("lists a scatter's points, with their series when there are several", () => {
    const one = chartTable(chart({ kind: "scatter", series: [{ data: [[1, 2], [3, 4]] }] }));
    expect(one.columns.map((c) => c.label)).toEqual(["x", "y"]);
    expect(one.rows).toEqual([
      [1, 2],
      [3, 4],
    ]);
    const two = chartTable(
      chart({ kind: "scatter", x_label: "Age", series: [{ name: "A", data: [[1, 2]] }, { data: [[5, 6]] }] }),
    );
    expect(two.columns.map((c) => c.label)).toEqual(["Series", "Age", "y"]);
    expect(two.rows).toEqual([
      ["A", 1, 2],
      ["Series 2", 5, 6],
    ]);
  });

  it("is a table the table renderer accepts", () => {
    const table = chartTable(chart({ kind: "bar", labels: ["a"], series: [{ data: [1] }] }));
    expect(validateTable({ title: "t", columns: table.columns, rows: table.rows }).ok).toBe(true);
  });
});
