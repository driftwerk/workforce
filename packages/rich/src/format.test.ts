import { describe, expect, it } from "vitest";

import { autoFormat, formatNumber, looksLikeYears } from "./format";

describe("formatNumber", () => {
  it("groups thousands and keeps at most two decimals by default", () => {
    expect(formatNumber(523797.33)).toBe("523,797.33");
    expect(formatNumber(1234)).toBe("1,234");
    expect(formatNumber(157.3)).toBe("157.3");
    expect(formatNumber(1234.5678)).toBe("1,234.57");
    expect(formatNumber(0.5)).toBe("0.5");
    expect(formatNumber(999)).toBe("999");
  });

  it("puts the unit before the digits and the sign before the unit", () => {
    expect(formatNumber(1234.5, { unit: "£" })).toBe("£1,234.5");
    expect(formatNumber(-1234.5, { unit: "£" })).toBe("-£1,234.5");
    expect(formatNumber(12.5, { unit: "%" })).toBe("12.5%");
  });

  it("gives currency exactly two decimals", () => {
    expect(formatNumber(9400, { format: "currency", unit: "£" })).toBe("£9,400.00");
    expect(formatNumber(12800.5, { format: "currency", unit: "£" })).toBe("£12,800.50");
    expect(formatNumber(6250.999, { format: "currency" })).toBe("6,251.00");
  });

  it("rounds integers and percentages sensibly", () => {
    expect(formatNumber(1234.6, { format: "integer" })).toBe("1,235");
    expect(formatNumber(82.04, { format: "percent" })).toBe("82%");
    expect(formatNumber(82.45, { format: "percent" })).toBe("82.5%");
    expect(formatNumber(-3.25, { format: "percent" })).toBe("-3.3%");
  });

  it("leaves identifiers and text untouched", () => {
    expect(formatNumber(1024773, { format: "id" })).toBe("1024773");
    expect(formatNumber(2026, { format: "text" })).toBe("2026");
    expect(formatNumber(1024773, { format: "id", unit: "£" })).toBe("1024773");
  });

  it("passes non-finite values through", () => {
    expect(formatNumber(Number.NaN)).toBe("NaN");
    expect(formatNumber(Number.POSITIVE_INFINITY)).toBe("Infinity");
  });
});

describe("autoFormat", () => {
  it("treats reference-like labels as identifiers", () => {
    expect(autoFormat("Lead ID", [1024773])).toBe("id");
    expect(autoFormat("Reference", [1024773])).toBe("id");
    expect(autoFormat("Quote number", [1024773])).toBe("id");
    expect(autoFormat("Postcode", [1024773])).toBe("id");
    expect(autoFormat("Year", [2024, 2025])).toBe("id");
    expect(autoFormat("lead_id", [1024773])).toBe("id");
    expect(autoFormat("quote-number", [1024773])).toBe("id");
    expect(autoFormat("number_of_quotes", [1024773])).toBe("number");
  });

  it("treats counts and measures as numbers", () => {
    expect(autoFormat("Number of leads", [1427, 2024])).toBe("number");
    expect(autoFormat("Total fees", [523797.33])).toBe("number");
    expect(autoFormat("Days to complete", [157.3])).toBe("number");
    expect(autoFormat("Completed", [4132])).toBe("number");
  });

  it("recognises a column of calendar years by its values", () => {
    expect(autoFormat("Period", [2024, 2025, 2026])).toBe("id");
    expect(autoFormat("Period", [2024, 2025.5])).toBe("number");
    expect(autoFormat("Period", [1899, 2025])).toBe("number");
    expect(autoFormat("Period", [])).toBe("number");
  });
});

describe("looksLikeYears", () => {
  it("is true only for a non-empty run of integer years", () => {
    expect(looksLikeYears([2024, 2025])).toBe(true);
    expect(looksLikeYears([2024, 12])).toBe(false);
    expect(looksLikeYears([])).toBe(false);
  });
});
