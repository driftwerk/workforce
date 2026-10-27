import { describe, expect, it } from "vitest";

import { csvFilename, toCsv } from "./csv";

describe("toCsv", () => {
  it("quotes commas, quotes, and newlines per RFC 4180", () => {
    const csv = toCsv(["A", "B,x"], [['say "hi"', "line\nbreak"], [null, 7]]);
    expect(csv).toBe(
      "\uFEFF" + 'A,"B,x"\r\n"say ""hi""","line\nbreak"\r\n,7\r\n',
    );
  });

  it("starts with a UTF-8 BOM and uses CRLF endings", () => {
    const csv = toCsv(["A"], [[1]]);
    expect(csv.charCodeAt(0)).toBe(0xfeff);
    expect(csv.endsWith("\r\n")).toBe(true);
  });
});

describe("csvFilename", () => {
  it("slugs the title and falls back to table.csv", () => {
    expect(csvFilename("Q3 Invoices — Final!")).toBe("q3-invoices-final.csv");
    expect(csvFilename()).toBe("table.csv");
    expect(csvFilename("***")).toBe("table.csv");
  });
});
