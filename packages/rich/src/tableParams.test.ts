import { describe, expect, it } from "vitest";
import type { ColumnFiltersState } from "@tanstack/react-table";

import { clientType, toServerFilters } from "./tableParams";
import type { ArtifactColumn } from "./spec";

const columns: ArtifactColumn[] = [
  { key: "name", label: "Name", type: "text" },
  { key: "amount", label: "Amount", type: "number" },
  { key: "due", label: "Due", type: "date" },
];

describe("clientType", () => {
  it("maps the server 'text' type to the inline 'string' type", () => {
    expect(clientType("text")).toBe("string");
    expect(clientType("number")).toBe("number");
    expect(clientType("date")).toBe("date");
  });
});

describe("toServerFilters", () => {
  it("maps a text column filter to a contains op", () => {
    const filters: ColumnFiltersState = [{ id: "name", value: "acme" }];
    expect(toServerFilters(filters, columns)).toEqual([{ col: "name", op: "contains", value: "acme" }]);
  });

  it("maps a numeric range to gte/lte, dropping open bounds", () => {
    expect(toServerFilters([{ id: "amount", value: [100, 500] }], columns)).toEqual([
      { col: "amount", op: "gte", value: 100 },
      { col: "amount", op: "lte", value: 500 },
    ]);
    expect(toServerFilters([{ id: "amount", value: [null, 500] }], columns)).toEqual([
      { col: "amount", op: "lte", value: 500 },
    ]);
  });

  it("passes date bounds through as epoch millis", () => {
    const from = Date.parse("2026-07-01");
    expect(toServerFilters([{ id: "due", value: [from, null] }], columns)).toEqual([
      { col: "due", op: "gte", value: from },
    ]);
  });

  it("drops empty text filters and unknown columns", () => {
    expect(toServerFilters([{ id: "name", value: "" }], columns)).toEqual([]);
    expect(toServerFilters([{ id: "ghost", value: "x" }], columns)).toEqual([]);
  });
});
