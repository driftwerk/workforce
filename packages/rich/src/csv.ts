// CSV export of a rendered table view: RFC 4180 quoting, CRLF line endings,
// and a UTF-8 BOM so Excel opens it with the right encoding.

import type { Cell } from "./spec";

function field(cell: Cell): string {
  if (cell === null) return "";
  const s = String(cell);
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

export function toCsv(header: string[], rows: Cell[][]): string {
  const lines = [header.map(field), ...rows.map((r) => r.map(field))];
  return "\uFEFF" + lines.map((l) => l.join(",")).join("\r\n") + "\r\n";
}

/** Slug a table title into a filename, defaulting to `table.csv`. */
export function csvFilename(title?: string): string {
  const slug = (title ?? "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60);
  return `${slug || "table"}.csv`;
}

export function csvBlob(csv: string): Blob {
  return new Blob([csv], { type: "text/csv;charset=utf-8" });
}
