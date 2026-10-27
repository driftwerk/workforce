// The page request a server-backed table makes through the host, and the pure
// translation from the table's state into it. Kept out of the component so it
// is unit-tested directly.

import type { ColumnFiltersState } from "@tanstack/react-table";

import type { ArtifactColumn, ArtifactColumnType, ColumnType } from "./spec";

/** One filter on a server-backed table page. */
export interface TableFilterParam {
  col: string;
  op: "contains" | "gte" | "lte";
  value: string | number;
}

/** A page request for a server-backed (artifact) table. */
export interface TableParams {
  offset: number;
  limit: number;
  sort?: { column: string; desc: boolean };
  search?: string;
  filters?: TableFilterParam[];
}

/** The query string the staged-table route takes for a page, for hosts that
 *  serve it over HTTP. */
export function tableQueryString(p: TableParams): string {
  const q = new URLSearchParams();
  q.set("offset", String(p.offset));
  q.set("limit", String(p.limit));
  if (p.sort) {
    q.set("sort", p.sort.column);
    q.set("dir", p.sort.desc ? "desc" : "asc");
  }
  if (p.search) q.set("q", p.search);
  if (p.filters && p.filters.length) q.set("filters", JSON.stringify(p.filters));
  return q.toString();
}

/** The server's `"text"` maps to the inline table's `"string"`. */
export function clientType(t: ArtifactColumnType): ColumnType {
  return t === "text" ? "string" : t;
}

/** Translate TanStack column-filter state into server filter params: text → a
 *  `contains`, a numeric/date `[min, max]` → `gte`/`lte` (date bounds are epoch
 *  millis, as {@link RangeInputs} produces). Filters on unknown columns drop. */
export function toServerFilters(
  filters: ColumnFiltersState,
  columns: ArtifactColumn[],
): TableFilterParam[] {
  const typeOf = (id: string) => columns.find((c) => c.key === id)?.type;
  return filters.flatMap((f): TableFilterParam[] => {
    const type = typeOf(f.id);
    if (type === undefined) return []; // filter for a column not in the schema
    if (type === "text") {
      return typeof f.value === "string" && f.value
        ? [{ col: f.id, op: "contains", value: f.value }]
        : [];
    }
    const [min, max] = (f.value as [number | null, number | null] | undefined) ?? [null, null];
    const out: TableFilterParam[] = [];
    if (min != null) out.push({ col: f.id, op: "gte", value: min });
    if (max != null) out.push({ col: f.id, op: "lte", value: max });
    return out;
  });
}
