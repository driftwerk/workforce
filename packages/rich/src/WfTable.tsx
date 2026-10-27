// The wf-table renderer. Two data sources behind one presentation: a small
// inline table sorts/filters/pages client-side; a large artifact-backed table
// does it server-side (TanStack manual models + a debounced page fetch). The
// header, sort affordance, filter row, pager, and empty state are shared.

import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  columnFilteringFeature,
  createFilteredRowModel,
  createPaginatedRowModel,
  createSortedRowModel,
  globalFilteringFeature,
  metaHelper,
  rowPaginationFeature,
  rowSortingFeature,
  sortFn_basic,
  tableFeatures,
  useTable,
  type Column,
  type ColumnDef,
  type ColumnFiltersState,
  type FilterFn,
  type PaginationState,
  type ReactTable,
  type Row,
  type SortingState,
} from "@tanstack/react-table";

import {
  cellValue,
  inferColumnTypes,
  type ArtifactTableSpec,
  type Cell,
  type ColumnType,
  type InlineTableSpec,
  type TableColumn,
  type TablePage,
  type TableSpec,
} from "./spec";
import { autoFormat, formatNumber, type NumberStyle } from "./format";
import { csvBlob, csvFilename, toCsv } from "./csv";
import { TableExpired, useHost, useSaveFile } from "./host";
import { clientType, toServerFilters, type TableParams } from "./tableParams";

type TableRow = Cell[];

// One feature set for both sources: an artifact table sets the `manual*` flags,
// which bypass these client row models, and pages on the server instead.
const features = tableFeatures({
  columnFilteringFeature,
  globalFilteringFeature,
  rowSortingFeature,
  rowPaginationFeature,
  filteredRowModel: createFilteredRowModel(),
  sortedRowModel: createSortedRowModel(),
  paginatedRowModel: createPaginatedRowModel(),
  sortFns: { basic: sortFn_basic },
  columnMeta: metaHelper<{ type: ColumnType; align: "left" | "right" }>(),
});

type Features = typeof features;

const INLINE_PAGE_SIZES = [10, 25, 50];
const ARTIFACT_PAGE_SIZES = [25, 50, 100];
const FETCH_DEBOUNCE_MS = 250;
const NO_ROWS: TableRow[] = [];

/** The cell's raw text — what search matches and CSV exports. */
function display(cell: Cell): string {
  if (cell === null) return "";
  if (typeof cell === "boolean") return cell ? "true" : "false";
  return String(cell);
}

/** The cell as a person reads it: numeric columns get separators and sensible
 *  decimals under the column's style; everything else is its raw text. */
export function displayCell(cell: Cell, type: ColumnType, style?: NumberStyle): string {
  if (type === "number") {
    const n = cellValue(cell, "number");
    if (typeof n === "number") return formatNumber(n, style);
  }
  return display(cell);
}

/** The style a numeric column renders with: the agent's declaration, else an
 *  inference from the label and the values on show. */
export function columnStyle(
  col: Pick<TableColumn, "label" | "format" | "unit"> | undefined,
  type: ColumnType,
  rows: readonly TableRow[],
  index: number,
): NumberStyle | undefined {
  if (type !== "number") return undefined;
  const values: number[] = [];
  for (const row of rows) {
    const n = cellValue(row[index], "number");
    if (typeof n === "number") values.push(n);
  }
  return { format: col?.format ?? autoFormat(col?.label ?? "", values), unit: col?.unit };
}

const alignFor = (type: ColumnType): "left" | "right" => (type === "number" ? "right" : "left");

/** Range filter over the column's typed value (numbers, or dates as epoch ms).
 *  Empty cells (accessor `undefined`) never fall inside a range. */
const rangeFilter: FilterFn<Features, TableRow> = (row, columnId, value: [number | null, number | null]) => {
  const v = row.getValue<number | undefined>(columnId);
  if (typeof v !== "number") return false;
  const [min, max] = value;
  return (min === null || v >= min) && (max === null || v <= max);
};

const textFilter: FilterFn<Features, TableRow> = (row, columnId, value: string) => {
  const v = row.getValue<string | undefined>(columnId);
  return v != null && v.toLowerCase().includes(value.toLowerCase());
};

/** Global search matches either the raw or the formatted text of the one column
 *  it is called for (inline column ids are the row index), so "523797" and
 *  "523,797" both find the row; TanStack ORs it across columns. */
function globalSearchFor(types: ColumnType[], styles: (NumberStyle | undefined)[]): FilterFn<Features, TableRow> {
  return (row, columnId, value: string) => {
    const i = Number(columnId);
    const cell = row.original[i];
    if (cell == null) return false;
    const needle = value.toLowerCase();
    return (
      display(cell).toLowerCase().includes(needle) ||
      displayCell(cell, types[i], styles[i]).toLowerCase().includes(needle)
    );
  };
}

function RangeInputs({ column, type }: { column: Column<Features, TableRow>; type: ColumnType }) {
  const inputType = type === "date" ? "date" : "number";
  const parse = (raw: string): number | null => {
    if (!raw) return null;
    const n = type === "date" ? Date.parse(raw) : Number(raw);
    return Number.isNaN(n) ? null : n;
  };
  // A date-typed "to" bound means end of that day, so same-day ranges match.
  const set = (index: 0 | 1) => (raw: string) => {
    const bound = parse(raw);
    const adjusted = bound !== null && type === "date" && index === 1 ? bound + 86_399_999 : bound;
    const current = (column.getFilterValue() as [number | null, number | null]) ?? [null, null];
    const next: [number | null, number | null] = [...current];
    next[index] = adjusted;
    column.setFilterValue(next[0] === null && next[1] === null ? undefined : next);
  };
  return (
    <span className="wf-rt-range">
      <input type={inputType} className="wf-rt-filter" placeholder="from" aria-label="Filter from"
        onChange={(e) => set(0)(e.target.value)} />
      <input type={inputType} className="wf-rt-filter" placeholder="to" aria-label="Filter to"
        onChange={(e) => set(1)(e.target.value)} />
    </span>
  );
}

interface Footer {
  count: number;
  total?: number;
  note?: string;
}

/** The shared table presentation, driven entirely by a TanStack `table`. The
 *  data source (client rows vs a fetched page) is wired by the caller. */
function TableChrome({
  table,
  title,
  columnCount,
  showSearch,
  paged,
  pageSizes,
  footer,
  busy,
  onCsv,
  lockKey,
}: {
  table: ReactTable<Features, TableRow>;
  title?: string;
  columnCount: number;
  showSearch: boolean;
  paged: boolean;
  pageSizes: number[];
  footer: Footer;
  busy?: boolean;
  onCsv?: () => void;
  lockKey: unknown;
}) {
  const [showFilters, setShowFilters] = useState(false);
  // Lock the scroll area to its first-rendered height so filtering to fewer rows
  // leaves empty space instead of shrinking the component and jumping the message.
  const tableRef = useRef<HTMLTableElement>(null);
  const [lockHeight, setLockHeight] = useState<number>();
  useLayoutEffect(() => {
    const h = tableRef.current?.offsetHeight;
    if (h) setLockHeight(h);
  }, [lockKey]);

  const search = (table.state.globalFilter as string) ?? "";
  const { pageIndex, pageSize } = table.state.pagination;
  const rows = table.getRowModel().rows;

  return (
    <div className="wf-rt">
      {title && <div className="wf-rt-title">{title}</div>}
      {showSearch && (
        <div className="wf-rt-bar">
          <input className="wf-rt-search" value={search} placeholder="Search…" aria-label="Search table"
            onChange={(e) => table.setGlobalFilter(e.target.value)} />
          <button type="button" className={"wf-rt-btn" + (showFilters ? " wf-rt-on" : "")}
            aria-pressed={showFilters} onClick={() => setShowFilters((f) => !f)}>
            Filters
          </button>
        </div>
      )}
      <div className="wf-tablewrap" style={lockHeight ? { minHeight: lockHeight } : undefined}>
        <table ref={tableRef}>
          <thead>
            {table.getHeaderGroups().map((hg) => (
              <tr key={hg.id}>
                {hg.headers.map((h) => {
                  const meta = h.column.columnDef.meta!;
                  const dir = h.column.getIsSorted();
                  return (
                    <th key={h.id} style={{ textAlign: meta.align }}
                      aria-sort={dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none"}>
                      <button type="button" className="wf-rt-sort"
                        onClick={h.column.getToggleSortingHandler()}
                        aria-label={`Sort by ${h.column.columnDef.header}`}>
                        <table.FlexRender header={h} />
                        <span className="wf-rt-dir" aria-hidden="true">
                          {dir === "asc" ? "▲" : dir === "desc" ? "▼" : ""}
                        </span>
                      </button>
                    </th>
                  );
                })}
              </tr>
            ))}
            {showFilters && (
              <tr className="wf-rt-filters">
                {table.getFlatHeaders().map((h) => {
                  const meta = h.column.columnDef.meta!;
                  return (
                    <th key={h.id}>
                      {meta.type === "string" ? (
                        <input className="wf-rt-filter" placeholder="contains…"
                          aria-label={`Filter ${h.column.columnDef.header}`}
                          value={(h.column.getFilterValue() as string) ?? ""}
                          onChange={(e) => h.column.setFilterValue(e.target.value || undefined)} />
                      ) : (
                        <RangeInputs column={h.column} type={meta.type} />
                      )}
                    </th>
                  );
                })}
              </tr>
            )}
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.id}>
                {row.getAllCells().map((cell) => {
                  const meta = cell.column.columnDef.meta!;
                  return (
                    <td key={cell.id} style={{ textAlign: meta.align }}>
                      <table.FlexRender cell={cell} />
                    </td>
                  );
                })}
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td className="wf-rt-empty" colSpan={columnCount}>
                  {busy ? "Loading…" : "No matching rows"}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="wf-rt-foot">
        <span className="wf-rt-count" aria-live="polite">
          {busy
            ? "Updating…"
            : footer.total !== undefined && footer.total !== footer.count
              ? `${footer.count.toLocaleString()} of ${footer.total.toLocaleString()} rows`
              : `${footer.count.toLocaleString()} rows`}
          {footer.note ? ` · ${footer.note}` : ""}
        </span>
        {paged && (
          <span className="wf-rt-pager">
            <select className="wf-rt-size" value={pageSize} aria-label="Rows per page"
              onChange={(e) => table.setPageSize(Number(e.target.value))}>
              {pageSizes.map((s) => (
                <option key={s} value={s}>{s} / page</option>
              ))}
            </select>
            <button type="button" className="wf-rt-btn" disabled={!table.getCanPreviousPage()}
              aria-label="Previous page" onClick={() => table.previousPage()}>
              ‹
            </button>
            <span className="wf-rt-page">{pageIndex + 1} / {Math.max(1, table.getPageCount())}</span>
            <button type="button" className="wf-rt-btn" disabled={!table.getCanNextPage()}
              aria-label="Next page" onClick={() => table.nextPage()}>
              ›
            </button>
          </span>
        )}
        {onCsv && (
          <button type="button" className="wf-rt-btn" onClick={onCsv}>⬇ CSV</button>
        )}
      </div>
    </div>
  );
}

function InlineTable({ spec, truncated }: { spec: InlineTableSpec; truncated?: string }) {
  const types = useMemo(() => inferColumnTypes(spec), [spec]);
  const styles = useMemo(
    () => spec.columns.map((col, i) => columnStyle(col, types[i], spec.rows, i)),
    [spec, types],
  );
  const globalSearch = useMemo(() => globalSearchFor(types, styles), [types, styles]);
  const columns = useMemo<ColumnDef<Features, TableRow>[]>(
    () =>
      spec.columns.map((col, i) => {
        const type = types[i];
        const style = styles[i];
        return {
          id: String(i),
          header: col.label,
          // `undefined` (not null) for empties so `sortUndefined` sorts them last.
          accessorFn: (row: TableRow) => cellValue(row[i], type) ?? undefined,
          cell: ({ row }: { row: Row<Features, TableRow> }) => displayCell(row.original[i], type, style),
          filterFn: type === "string" ? textFilter : rangeFilter,
          sortFn: "basic",
          sortUndefined: "last",
          meta: { type, align: col.align ?? alignFor(type) },
        };
      }),
    [spec, types, styles],
  );

  const table = useTable({
    features,
    data: spec.rows,
    columns,
    globalFilterFn: globalSearch,
    getColumnCanGlobalFilter: () => true,
    initialState: { pagination: { pageIndex: 0, pageSize: INLINE_PAGE_SIZES[0] } },
  });

  const save = useSaveFile();
  const total = spec.rows.length;
  const filtered = table.getPrePaginatedRowModel().rows;
  const exportCsv = () =>
    save(
      csvBlob(toCsv(spec.columns.map((c) => c.label), filtered.map((r) => r.original))),
      csvFilename(spec.title),
    );

  return (
    <TableChrome
      table={table}
      title={spec.title}
      columnCount={spec.columns.length}
      showSearch={total > 8}
      paged={total > INLINE_PAGE_SIZES[0]}
      pageSizes={INLINE_PAGE_SIZES}
      footer={{ count: filtered.length, total, note: truncated }}
      onCsv={exportCsv}
      lockKey={spec}
    />
  );
}

function ArtifactTable({ spec }: { spec: ArtifactTableSpec }) {
  const { latest, tables } = useHost();
  const [page, setPage] = useState<TablePage | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error" | "expired" | "unsupported">(
    "loading",
  );
  const [busy, setBusy] = useState(false);
  const [retry, setRetry] = useState(0);
  const [sorting, setSorting] = useState<SortingState>([]);
  const [columnFilters, setColumnFilters] = useState<ColumnFiltersState>([]);
  const [globalFilter, setGlobalFilter] = useState("");
  const [pagination, setPagination] = useState<PaginationState>({ pageIndex: 0, pageSize: ARTIFACT_PAGE_SIZES[0] });
  const seq = useRef(0);
  // Only a filter/search change debounces; sort/page/size fetch immediately.
  const filterKey = JSON.stringify([columnFilters, globalFilter]);
  const prevFilterKey = useRef(filterKey);
  const toFirstPage = (p: PaginationState): PaginationState => ({ ...p, pageIndex: 0 });

  const columns = useMemo<ColumnDef<Features, TableRow>[]>(
    () =>
      (page?.columns ?? []).map((col, i) => {
        const type = clientType(col.type);
        const overlay = spec.columns?.find((c) => c.key === col.key);
        const style = columnStyle(
          { label: overlay?.label ?? col.label, format: overlay?.format, unit: overlay?.unit },
          type,
          page?.rows ?? NO_ROWS,
          i,
        );
        return {
          id: col.key,
          header: overlay?.label ?? col.label,
          accessorFn: (row: TableRow) => row[i] ?? undefined,
          cell: ({ row }: { row: Row<Features, TableRow> }) => displayCell(row.original[i], type, style),
          meta: { type, align: overlay?.align ?? alignFor(type) },
        };
      }),
    // Rows only feed the year/id inference; the page's columns are the real key.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [page?.columns, spec.columns],
  );

  const table = useTable({
    features,
    data: page?.rows ?? NO_ROWS,
    columns,
    rowCount: page?.total ?? 0,
    manualPagination: true,
    manualSorting: true,
    manualFiltering: true,
    enableMultiSort: false,
    state: { sorting, columnFilters, globalFilter, pagination },
    onSortingChange: setSorting,
    onColumnFiltersChange: (u) => {
      setColumnFilters(u);
      setPagination(toFirstPage);
    },
    onGlobalFilterChange: (u) => {
      setGlobalFilter(u);
      setPagination(toFirstPage);
    },
    onPaginationChange: setPagination,
  });

  useEffect(() => {
    const host = latest.current;
    const fetchPage = host.fetchTablePage?.bind(host);
    if (!fetchPage) {
      setStatus("unsupported");
      return;
    }
    const mine = ++seq.current;
    setBusy(true);
    const debounce = page && filterKey !== prevFilterKey.current ? FETCH_DEBOUNCE_MS : 0;
    prevFilterKey.current = filterKey;
    const handle = setTimeout(() => {
      const params: TableParams = {
        offset: pagination.pageIndex * pagination.pageSize,
        limit: pagination.pageSize,
        sort: sorting[0] ? { column: sorting[0].id, desc: sorting[0].desc } : undefined,
        search: globalFilter.trim() || undefined,
        filters: toServerFilters(columnFilters, page?.columns ?? []),
      };
      fetchPage(spec.artifactId, params)
        .then((p) => {
          if (mine !== seq.current) return;
          setPage(p);
          setStatus("ready");
          setBusy(false);
        })
        .catch((e) => {
          if (mine !== seq.current) return;
          setStatus(e instanceof TableExpired ? "expired" : "error");
          setBusy(false);
        });
    }, debounce);
    return () => clearTimeout(handle);
    // `page` is read for its column types only; adding it would refetch on every
    // load. The server schema is stable, so the closure's value is always current.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [latest, tables, spec.artifactId, sorting, columnFilters, globalFilter, pagination, retry]);

  if (status === "unsupported") {
    return <div className="wf-rt-msg" role="alert">This table can&apos;t be shown here.</div>;
  }
  if (status === "expired") {
    return <div className="wf-rt-msg" role="alert">This table is no longer available — ask again to reload it.</div>;
  }
  if (status === "error") {
    return (
      <div className="wf-rt-msg" role="alert">
        Couldn&apos;t load this table.{" "}
        <button type="button" className="wf-rt-btn" onClick={() => { setStatus("loading"); setRetry((n) => n + 1); }}>
          Retry
        </button>
      </div>
    );
  }
  if (!page) {
    return <div className="wf-skel wf-skel-table" role="status"><span className="wf-skel-label">Loading table…</span></div>;
  }
  return (
    <TableChrome
      table={table}
      title={spec.title}
      columnCount={page.columns.length}
      showSearch
      paged={page.total > ARTIFACT_PAGE_SIZES[0]}
      pageSizes={ARTIFACT_PAGE_SIZES}
      footer={{ count: page.total }}
      busy={busy}
      lockKey={spec.artifactId}
    />
  );
}

export default function WfTable({
  spec,
  truncated,
}: {
  spec: TableSpec;
  truncated?: string;
}): ReactNode {
  return spec.mode === "artifact" ? (
    <ArtifactTable spec={spec} />
  ) : (
    <InlineTable spec={spec} truncated={truncated} />
  );
}
