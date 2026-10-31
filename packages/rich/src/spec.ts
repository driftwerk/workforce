// The wf-table / wf-chart / wf-form / wf-map / wf-diagram payload contract: lenient parse, strict
// validation. The payload is agent-generated and therefore untrusted — every
// shape error degrades to the plain code-block fallback, never a crash. Only
// scalar data survives validation; no formatter strings or callbacks ever reach
// a renderer.

import { isGeographyId, type GeographyId } from "./geo/ids";
import { COLUMN_FORMATS, type ColumnFormat } from "./format";

export type Cell = string | number | boolean | null;

export interface TableColumn {
  key: string;
  label: string;
  align?: "left" | "right";
  /** How numeric cells display; inferred from the label and values when absent. */
  format?: ColumnFormat;
  /** A prefix such as `£` (or `%`, applied as a suffix) for numeric cells. */
  unit?: string;
}

/** A small table whose rows are inlined in the payload — rendered client-side. */
export interface InlineTableSpec {
  mode: "inline";
  title?: string;
  columns: TableColumn[];
  rows: Cell[][];
}

/** A large table staged server-side; the host pages it by `artifactId`.
 *  `columns`, when given, overlays label/align/format/unit onto the server's
 *  columns by key — the server infers types but knows nothing about money. */
export interface ArtifactTableSpec {
  mode: "artifact";
  title?: string;
  artifactId: string;
  columns?: TableColumn[];
}

export type TableSpec = InlineTableSpec | ArtifactTableSpec;

/** Column types as the server reports them — note `"text"` where an inline table
 *  uses `"string"` (see {@link ColumnType}). */
export type ArtifactColumnType = "text" | "number" | "date";

/** One column of a server-backed table, with its server-inferred type. */
export interface ArtifactColumn {
  key: string;
  label: string;
  type: ArtifactColumnType;
}

/** One page of a server-backed table. */
export interface TablePage {
  columns: ArtifactColumn[];
  rows: Cell[][];
  total: number;
}

const ARTIFACT_ID = /^art_[0-9a-f]{32}$/;

export type ChartKind = "pie" | "bar" | "line" | "area" | "scatter";

export interface ChartSeries {
  name?: string;
  data: number[] | [number, number][];
}

/** The media type of a chart's validated spec delivered as a file: a host
 *  that receives one draws it with {@link validateChart} and the chart block. */
export const CHART_SPEC_MEDIA_TYPE = "application/vnd.workforce.chart+json";

export interface ChartSpec {
  kind: ChartKind;
  title?: string;
  labels?: string[];
  series: ChartSeries[];
  stacked?: boolean;
  horizontal?: boolean;
  x_label?: string;
  y_label?: string;
  unit?: string;
}

export type ColumnType = "string" | "number" | "date";

export interface MapRegion {
  name: string;
  value: number;
}

export interface MapSpec {
  kind: "choropleth";
  geo: GeographyId;
  regions: MapRegion[];
  title?: string;
  value_label?: string;
  unit?: string;
  min?: number;
  max?: number;
}

export type Validated<T> =
  | { ok: true; spec: T; truncated?: string }
  | { ok: false; error: string };

export const MAX_ROWS = 2000;
export const MAX_COLUMNS = 40;
export const MAX_SERIES = 8;
export const MAX_POINTS = 1000;
export const MAX_REGIONS = 1000;

/** Strip trailing commas outside string literals — the one JSON slip models
 *  make often enough to be worth recovering from. */
function stripTrailingCommas(s: string): string {
  let out = "";
  let inString = false;
  let escaped = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (inString) {
      out += c;
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') inString = false;
      continue;
    }
    if (c === '"') {
      inString = true;
      out += c;
      continue;
    }
    if (c === ",") {
      let j = i + 1;
      while (j < s.length && /\s/.test(s[j])) j++;
      if (s[j] === "}" || s[j] === "]") continue;
    }
    out += c;
  }
  return out;
}

/** Parse a fence payload: strict JSON first, then one trailing-comma-lenient
 *  retry. `null` when neither parses (mid-stream or genuinely malformed). */
export function parsePayload(text: string): unknown {
  for (const candidate of [text, stripTrailingCommas(text)]) {
    try {
      return JSON.parse(candidate);
    } catch {
      /* try the next candidate */
    }
  }
  return null;
}

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

const isCell = (v: unknown): v is Cell =>
  v === null || typeof v === "string" || typeof v === "number" || typeof v === "boolean";

const optionalString = (v: unknown): string | undefined =>
  typeof v === "string" && v.trim() ? v : undefined;

function toColumn(v: unknown, index: number): TableColumn | null {
  if (typeof v === "string") {
    const key = v.trim() || `Column ${index + 1}`;
    return { key, label: key };
  }
  if (isRecord(v) && typeof v.key === "string" && v.key.trim()) {
    const format = COLUMN_FORMATS.find((f) => f === v.format);
    return {
      key: v.key,
      label: typeof v.label === "string" && v.label.trim() ? v.label : v.key,
      align: v.align === "right" ? "right" : v.align === "left" ? "left" : undefined,
      ...(format ? { format } : {}),
      ...(typeof v.unit === "string" && v.unit.trim() ? { unit: v.unit.trim() } : {}),
    };
  }
  return null;
}

function toColumns(value: unknown): TableColumn[] | string {
  if (!Array.isArray(value) || value.length === 0) return "columns must be a non-empty array";
  if (value.length > MAX_COLUMNS) return `too many columns (max ${MAX_COLUMNS})`;
  const columns: TableColumn[] = [];
  for (const [i, c] of value.entries()) {
    const col = toColumn(c, i);
    if (!col) return `column ${i + 1} must be a name or {key,label}`;
    columns.push(col);
  }
  return columns;
}

export function validateTable(value: unknown): Validated<TableSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  // A large result staged server-side is referenced by artifact id; the host
  // fetches its columns and rows a page at a time.
  if (typeof value.artifact_id === "string" && ARTIFACT_ID.test(value.artifact_id)) {
    const overlay = value.columns === undefined ? undefined : toColumns(value.columns);
    if (typeof overlay === "string") return { ok: false, error: overlay };
    return {
      ok: true,
      spec: {
        mode: "artifact",
        title: optionalString(value.title),
        artifactId: value.artifact_id,
        ...(overlay ? { columns: overlay } : {}),
      },
    };
  }
  const columns = toColumns(value.columns);
  if (typeof columns === "string") return { ok: false, error: columns };
  if (!Array.isArray(value.rows)) return { ok: false, error: "rows must be an array" };
  const rows: Cell[][] = [];
  for (const [i, r] of value.rows.entries()) {
    if (rows.length === MAX_ROWS) break;
    if (!Array.isArray(r)) return { ok: false, error: `row ${i + 1} must be an array` };
    if (!r.every(isCell)) {
      return { ok: false, error: `row ${i + 1} has a non-scalar cell` };
    }
    // Ragged rows normalize to the column count: pad short, drop extras.
    const cells = r.slice(0, columns.length) as Cell[];
    while (cells.length < columns.length) cells.push(null);
    rows.push(cells);
  }
  const truncated =
    value.rows.length > MAX_ROWS
      ? `Showing the first ${MAX_ROWS.toLocaleString()} of ${value.rows.length.toLocaleString()} rows`
      : undefined;
  return {
    ok: true,
    spec: { mode: "inline", title: optionalString(value.title), columns, rows },
    truncated,
  };
}

const CHART_KINDS: readonly ChartKind[] = ["pie", "bar", "line", "area", "scatter"];

/** Numbers pass through; numeric strings (an LLM habit) coerce; all else fails. */
function toFiniteNumber(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function toSeries(v: unknown, index: number, scatter: boolean): ChartSeries | string {
  if (!isRecord(v) || !Array.isArray(v.data)) {
    return `series ${index + 1} must be an object with a data array`;
  }
  if (v.data.length === 0) return `series ${index + 1} has no data`;
  if (v.data.length > MAX_POINTS) {
    return `series ${index + 1} exceeds ${MAX_POINTS} points`;
  }
  const name = optionalString(v.name);
  if (scatter) {
    const pairs: [number, number][] = [];
    for (const p of v.data) {
      if (!Array.isArray(p) || p.length !== 2) {
        return `series ${index + 1}: scatter points must be [x,y] pairs`;
      }
      const x = toFiniteNumber(p[0]);
      const y = toFiniteNumber(p[1]);
      if (x === null || y === null) {
        return `series ${index + 1}: scatter points must be numeric`;
      }
      pairs.push([x, y]);
    }
    return { name, data: pairs };
  }
  const points: number[] = [];
  for (const p of v.data) {
    const n = toFiniteNumber(p);
    if (n === null) return `series ${index + 1}: data must be numbers`;
    points.push(n);
  }
  return { name, data: points };
}

export function validateChart(value: unknown): Validated<ChartSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  const kind = value.kind;
  if (typeof kind !== "string" || !CHART_KINDS.includes(kind as ChartKind)) {
    return { ok: false, error: `kind must be one of ${CHART_KINDS.join(", ")}` };
  }
  if (!Array.isArray(value.series) || value.series.length === 0) {
    return { ok: false, error: "series must be a non-empty array" };
  }
  if (value.series.length > MAX_SERIES) {
    return { ok: false, error: `too many series (max ${MAX_SERIES})` };
  }
  const scatter = kind === "scatter";
  const series: ChartSeries[] = [];
  for (const [i, s] of value.series.entries()) {
    const parsed = toSeries(s, i, scatter);
    if (typeof parsed === "string") return { ok: false, error: parsed };
    series.push(parsed);
  }
  let labels: string[] | undefined;
  if (value.labels !== undefined) {
    if (!Array.isArray(value.labels) || !value.labels.every(isCell)) {
      return { ok: false, error: "labels must be an array of names" };
    }
    labels = value.labels.map((l) => String(l ?? ""));
  }
  if (kind === "pie") {
    if (series.length !== 1) return { ok: false, error: "a pie chart takes exactly one series" };
    if (!labels) return { ok: false, error: "a pie chart needs labels for its slices" };
    if (labels.length !== series[0].data.length) {
      return { ok: false, error: "pie labels and data must be the same length" };
    }
  } else if (!scatter && labels) {
    for (const [i, s] of series.entries()) {
      if (s.data.length !== labels.length) {
        return { ok: false, error: `series ${i + 1} length does not match labels` };
      }
    }
  }
  const spec: ChartSpec = {
    kind: kind as ChartKind,
    title: optionalString(value.title),
    labels: scatter ? undefined : labels,
    series,
    stacked: value.stacked === true && kind !== "pie" && kind !== "scatter",
    horizontal: value.horizontal === true && kind === "bar",
    x_label: optionalString(value.x_label),
    y_label: optionalString(value.y_label),
    unit: optionalString(value.unit),
  };
  return { ok: true, spec };
}

export function validateMap(value: unknown): Validated<MapSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  if (value.kind !== "choropleth") return { ok: false, error: 'kind must be "choropleth"' };
  if (typeof value.geo !== "string" || !isGeographyId(value.geo)) {
    return { ok: false, error: "geo must be a known geography" };
  }
  if (!Array.isArray(value.regions) || value.regions.length === 0) {
    return { ok: false, error: "regions must be a non-empty array" };
  }
  if (value.regions.length > MAX_REGIONS) {
    return { ok: false, error: `too many regions (max ${MAX_REGIONS})` };
  }
  const regions: MapRegion[] = [];
  for (const [i, r] of value.regions.entries()) {
    if (!isRecord(r) || typeof r.name !== "string" || !r.name.trim()) {
      return { ok: false, error: `region ${i + 1} needs a name` };
    }
    const n = toFiniteNumber(r.value);
    if (n === null) return { ok: false, error: `region ${i + 1} needs a numeric value` };
    regions.push({ name: r.name.trim(), value: n });
  }
  const spec: MapSpec = {
    kind: "choropleth",
    geo: value.geo,
    regions,
    title: optionalString(value.title),
    value_label: optionalString(value.value_label),
    unit: optionalString(value.unit),
    min: toFiniteNumber(value.min) ?? undefined,
    max: toFiniteNumber(value.max) ?? undefined,
  };
  return { ok: true, spec };
}

const ISO_DATE = /^\d{4}-\d{2}-\d{2}([T ].*)?$/;

/** Per-column type inference: `number` when every non-null cell is numeric,
 *  `date` when every non-null cell is an ISO date, else `string`. Drives sort
 *  comparators, filter widgets, and numeric right-alignment. */
export function inferColumnTypes(spec: InlineTableSpec): ColumnType[] {
  return spec.columns.map((_, col) => {
    let seen = 0;
    let numbers = 0;
    let dates = 0;
    for (const row of spec.rows) {
      const cell = row[col];
      if (cell === null || cell === "") continue;
      seen++;
      if (typeof cell === "number" || (typeof cell === "string" && toFiniteNumber(cell) !== null)) {
        numbers++;
      } else if (
        typeof cell === "string" &&
        ISO_DATE.test(cell) &&
        !Number.isNaN(Date.parse(cell))
      ) {
        dates++;
      }
    }
    if (seen === 0) return "string";
    if (numbers === seen) return "number";
    if (dates === seen) return "date";
    return "string";
  });
}

/** The sortable/filterable value of a cell under its column type. */
export function cellValue(cell: Cell, type: ColumnType): number | string | null {
  if (cell === null || cell === "") return null;
  if (type === "number") return toFiniteNumber(cell);
  if (type === "date") {
    const t = typeof cell === "string" ? Date.parse(cell) : NaN;
    return Number.isNaN(t) ? null : t;
  }
  return String(cell);
}

// ─── wf-form ───────────────────────────────────────────────────────────────
// The agent emits a constrained declarative form spec; the closed field
// vocabulary is the whitelist (no HTML/JS, no eval, no JSON-Schema engine). The
// same lenient-parse → strict-validate → degrade discipline as tables/charts.

export const MAX_FORM_FIELDS = 64;
export const MAX_OPTIONS = 200;

export type FormFieldType =
  | "text"
  | "textarea"
  | "email"
  | "number"
  | "date"
  | "select"
  | "checkbox"
  | "radio"
  | "hidden";

/** One choice in an inline `options` list; `label` defaults to `value`. */
export interface FormOption {
  value: string;
  label: string;
}

/** A server-backed option source: a spilled 2+-column tabular artifact whose
 *  `value`/`label` columns feed a typeahead (reusing the wf-table page route). */
export interface OptionsRef {
  artifactId: string;
  value: string;
  label: string;
}

/** A validated field. Only the keys relevant to `type` are populated; a bad
 *  `pattern` is dropped (degrades to no pattern) rather than kept. */
export interface FormField {
  name: string;
  label: string;
  type: FormFieldType;
  required: boolean;
  help?: string;
  placeholder?: string;
  /** Normalised prefill: a string for scalar fields, `string[]` for multi, a
   *  boolean for a single checkbox. */
  value?: string | string[] | boolean;
  /** A server-round-trip error to show under the field (drives the re-emit loop). */
  error?: string;
  minLength?: number;
  maxLength?: number;
  pattern?: string;
  rows?: number;
  min?: number;
  max?: number;
  step?: number;
  dateMin?: string;
  dateMax?: string;
  options?: FormOption[];
  optionsRef?: OptionsRef;
  multiple?: boolean;
}

export interface FormSpec {
  title?: string;
  submitLabel: string;
  note?: string;
  fields: FormField[];
}

/** The live value of one control: a string for scalar inputs, a boolean for a
 *  single checkbox, a `string[]` for a multi-select / checkbox group. */
export type FieldState = string | boolean | string[];

/** A single submitted form value. Scalars for text/number/checkbox/select-single;
 *  a string array for a multi-value select or checkbox group. */
export type SubmitValue = string | number | boolean | string[];

const FORM_FIELD_TYPES: readonly FormFieldType[] = [
  "text",
  "textarea",
  "email",
  "number",
  "date",
  "select",
  "checkbox",
  "radio",
  "hidden",
];

const ISO_DATE_ONLY = /^\d{4}-\d{2}-\d{2}$/;
const EMAIL = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/** A scalar prefill normalised to a string (numbers/booleans stringify); a
 *  non-scalar becomes `undefined` (no prefill). */
function toScalarValue(v: unknown): string | undefined {
  if (typeof v === "string") return v;
  if (typeof v === "number" && Number.isFinite(v)) return String(v);
  if (typeof v === "boolean") return v ? "true" : "false";
  return undefined;
}

/** A multi-value prefill: an array of scalars, or a lone scalar wrapped. */
function toArrayValue(v: unknown): string[] | undefined {
  if (Array.isArray(v)) {
    return v
      .filter((x): x is string | number => typeof x === "string" || typeof x === "number")
      .map(String);
  }
  const scalar = toScalarValue(v);
  return scalar ? [scalar] : undefined;
}

/** A non-negative integer bound (`min_length` / `max_length` / `rows`). */
function toBound(v: unknown): number | undefined {
  const n = toFiniteNumber(v);
  return n === null || n < 0 ? undefined : Math.floor(n);
}

function toIsoDate(v: unknown): string | undefined {
  return typeof v === "string" && ISO_DATE_ONLY.test(v) && !Number.isNaN(Date.parse(v))
    ? v
    : undefined;
}

/** One inline option: a bare string, or `{value, label?}` (value string/number). */
function toOption(v: unknown): FormOption | null {
  const raw = typeof v === "string" ? { value: v } : isRecord(v) ? v : null;
  if (!raw) return null;
  if (typeof raw.value !== "string" && typeof raw.value !== "number") return null;
  const value = String(raw.value).trim();
  if (!value) return null;
  const label = typeof raw.label === "string" && raw.label.trim() ? raw.label : value;
  return { value, label };
}

type OptionsParse = FormOption[] | null | { error: string };

/** Parse an `options` array. `null` = absent/empty (defer to options_ref); an
 *  `{error}` degrades the whole spec (a bad shape must never silently vanish). */
function parseOptions(raw: unknown, name: string): OptionsParse {
  if (raw === undefined || raw === null) return null;
  if (!Array.isArray(raw)) return { error: `field "${name}" options must be an array` };
  if (raw.length > MAX_OPTIONS) {
    return { error: `field "${name}" has too many options (max ${MAX_OPTIONS})` };
  }
  const out: FormOption[] = [];
  for (const [i, o] of raw.entries()) {
    const opt = toOption(o);
    if (!opt) return { error: `field "${name}" option ${i + 1} is invalid` };
    out.push(opt);
  }
  return out.length ? out : null;
}

const isOptionsError = (x: OptionsParse): x is { error: string } =>
  x !== null && !Array.isArray(x);

function toOptionsRef(v: unknown): OptionsRef | null {
  if (!isRecord(v)) return null;
  const { artifact_id, value, label } = v;
  if (typeof artifact_id !== "string" || !ARTIFACT_ID.test(artifact_id)) return null;
  if (typeof value !== "string" || !value.trim()) return null;
  if (typeof label !== "string" || !label.trim()) return null;
  return { artifactId: artifact_id, value, label };
}

/** Validate one field to a `FormField`, or return an error string to degrade. */
function toField(raw: unknown, index: number): FormField | string {
  if (!isRecord(raw)) return `field ${index + 1} must be an object`;
  const name = typeof raw.name === "string" ? raw.name.trim() : "";
  if (!name) return `field ${index + 1} needs a name`;
  if (typeof raw.type !== "string" || !FORM_FIELD_TYPES.includes(raw.type as FormFieldType)) {
    return `field "${name}" has an invalid type`;
  }
  const type = raw.type as FormFieldType;
  const field: FormField = {
    name,
    label: optionalString(raw.label) ?? name,
    type,
    required: raw.required === true,
    help: optionalString(raw.help),
    error: optionalString(raw.error),
    placeholder: optionalString(raw.placeholder),
  };

  switch (type) {
    case "text":
    case "textarea": {
      field.minLength = toBound(raw.min_length);
      field.maxLength = toBound(raw.max_length);
      field.value = toScalarValue(raw.value);
      if (type === "textarea") field.rows = toBound(raw.rows);
      // A pattern is data, not code — RegExp is not eval. Compile once to check it
      // is well-formed; a bad pattern is dropped so validation degrades to none.
      if (type === "text" && typeof raw.pattern === "string" && raw.pattern) {
        try {
          new RegExp(raw.pattern);
          field.pattern = raw.pattern;
        } catch {
          /* malformed pattern → no pattern */
        }
      }
      break;
    }
    case "email":
      field.value = toScalarValue(raw.value);
      break;
    case "number":
      field.min = toFiniteNumber(raw.min) ?? undefined;
      field.max = toFiniteNumber(raw.max) ?? undefined;
      field.step = toFiniteNumber(raw.step) ?? undefined;
      field.value = toScalarValue(raw.value);
      break;
    case "date":
      field.dateMin = toIsoDate(raw.min);
      field.dateMax = toIsoDate(raw.max);
      field.value = toScalarValue(raw.value);
      break;
    case "select": {
      const options = parseOptions(raw.options, name);
      if (isOptionsError(options)) return options.error;
      const ref = toOptionsRef(raw.options_ref);
      if (!options && !ref) return `field "${name}" (select) needs options or options_ref`;
      if (options) {
        field.options = options;
        field.multiple = raw.multiple === true;
      } else {
        // A server-backed typeahead resolves to a single chosen row.
        field.optionsRef = ref!;
      }
      field.value = field.multiple ? toArrayValue(raw.value) : toScalarValue(raw.value);
      break;
    }
    case "radio": {
      const options = parseOptions(raw.options, name);
      if (isOptionsError(options)) return options.error;
      if (!options) return `field "${name}" (radio) needs options`;
      field.options = options;
      field.value = toScalarValue(raw.value);
      break;
    }
    case "checkbox": {
      const options = parseOptions(raw.options, name);
      if (isOptionsError(options)) return options.error;
      if (options) {
        field.options = options;
        field.multiple = true;
        field.value = toArrayValue(raw.value);
      } else {
        field.value = raw.value === true;
      }
      break;
    }
    case "hidden": {
      const value = toScalarValue(raw.value);
      if (value === undefined) return `hidden field "${name}" needs a value`;
      field.value = value;
      break;
    }
  }
  return field;
}

export function validateForm(value: unknown): Validated<FormSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  if (!Array.isArray(value.fields) || value.fields.length === 0) {
    return { ok: false, error: "fields must be a non-empty array" };
  }
  if (value.fields.length > MAX_FORM_FIELDS) {
    return { ok: false, error: `too many fields (max ${MAX_FORM_FIELDS})` };
  }
  const fields: FormField[] = [];
  const names = new Set<string>();
  for (const [i, raw] of value.fields.entries()) {
    const field = toField(raw, i);
    if (typeof field === "string") return { ok: false, error: field };
    if (names.has(field.name)) return { ok: false, error: `duplicate field name "${field.name}"` };
    names.add(field.name);
    fields.push(field);
  }
  return {
    ok: true,
    spec: {
      title: optionalString(value.title),
      submitLabel: optionalString(value.submit_label) ?? "Submit",
      note: optionalString(value.note),
      fields,
    },
  };
}

/** Whether the closed vocabulary considers a value present. A `false` boolean is
 *  present (a valid answer); an empty string / empty array / unset is not. */
function isEmptyValue(v: FieldState): boolean {
  if (typeof v === "boolean") return false;
  if (Array.isArray(v)) return v.length === 0;
  return v.trim() === "";
}

const optionValues = (field: FormField): Set<string> =>
  new Set((field.options ?? []).map((o) => o.value));

/** Pure per-field validation: `null` when the value is acceptable, else a short
 *  human message. `options_ref` selects skip membership (the renderer only lets
 *  the user pick a returned row; the full set is validated server-side). */
export function fieldError(field: FormField, value: FieldState): string | null {
  if (field.required && isEmptyValue(value)) return "Required";
  if (isEmptyValue(value)) return null;
  switch (field.type) {
    case "text":
    case "textarea": {
      const s = String(value).trim();
      if (field.minLength !== undefined && s.length < field.minLength) {
        return `Must be at least ${field.minLength} characters`;
      }
      if (field.maxLength !== undefined && s.length > field.maxLength) {
        return `Must be at most ${field.maxLength} characters`;
      }
      if (field.pattern) {
        try {
          if (!new RegExp(field.pattern).test(s)) return "Invalid format";
        } catch {
          /* pattern already vetted at parse; treat a throw as no constraint */
        }
      }
      return null;
    }
    case "email":
      return EMAIL.test(String(value).trim()) ? null : "Enter a valid email address";
    case "number": {
      const s = String(value).trim();
      const n = Number(s);
      if (s === "" || !Number.isFinite(n)) return "Enter a number";
      if (field.min !== undefined && n < field.min) return `Must be at least ${field.min}`;
      if (field.max !== undefined && n > field.max) return `Must be at most ${field.max}`;
      return null;
    }
    case "date": {
      const s = String(value);
      if (!ISO_DATE_ONLY.test(s) || Number.isNaN(Date.parse(s))) return "Enter a valid date";
      if (field.dateMin && s < field.dateMin) return `Must be on or after ${field.dateMin}`;
      if (field.dateMax && s > field.dateMax) return `Must be on or before ${field.dateMax}`;
      return null;
    }
    case "select":
    case "checkbox":
    case "radio": {
      if (!field.options) return null;
      const allowed = optionValues(field);
      const chosen = Array.isArray(value) ? value : [String(value)];
      return chosen.every((v) => allowed.has(v)) ? null : "Select a valid option";
    }
    case "hidden":
      return null;
  }
}

/** The starting control value for a field, from its (normalised) prefill. */
export function initialFieldState(field: FormField): FieldState {
  const v = field.value;
  if (field.type === "checkbox" && !field.options) return v === true;
  if ((field.type === "checkbox" && field.options) || (field.type === "select" && field.multiple)) {
    return Array.isArray(v) ? [...v] : [];
  }
  return typeof v === "string" ? v : "";
}

export function initialFormState(spec: FormSpec): Record<string, FieldState> {
  const state: Record<string, FieldState> = {};
  for (const field of spec.fields) state[field.name] = initialFieldState(field);
  return state;
}

/** Every field's value is acceptable — gates the Submit button. */
export function formValid(spec: FormSpec, state: Record<string, FieldState>): boolean {
  return spec.fields.every((f) => fieldError(f, state[f.name]) === null);
}

/** Map one typeahead page's rows to options, resolving the value/label columns by
 *  the `options_ref` keys. A missing label column falls back to the value. */
export function optionsFromPage(page: TablePage, ref: OptionsRef): FormOption[] {
  const valueIdx = page.columns.findIndex((c) => c.key === ref.value);
  const labelIdx = page.columns.findIndex((c) => c.key === ref.label);
  if (valueIdx < 0) return [];
  const options: FormOption[] = [];
  for (const row of page.rows) {
    const rawValue = row[valueIdx];
    if (rawValue === null || rawValue === undefined) continue;
    const value = String(rawValue);
    const rawLabel = labelIdx >= 0 ? row[labelIdx] : null;
    options.push({ value, label: rawLabel == null ? value : String(rawLabel) });
  }
  return options;
}

/** The label to show for a chosen value: its inline option label, a resolved
 *  typeahead label, else the raw value. */
function labelFor(field: FormField, value: string, resolved?: Record<string, string>): string {
  const inline = field.options?.find((o) => o.value === value);
  if (inline) return inline.label;
  return resolved?.[value] ?? value;
}

/** A field's human-readable submitted value for the summary line. */
export function displayValue(
  field: FormField,
  value: FieldState,
  resolved?: Record<string, string>,
): string {
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (Array.isArray(value)) return value.map((v) => labelFor(field, v, resolved)).join(", ");
  return labelFor(field, value, resolved);
}

/** The readable summary persisted as the user's turn — `Submitted "<title>"`
 *  then a labelled line per non-empty visible field. Hidden fields (agent ids)
 *  are omitted so raw ids don't clutter the transcript. */
export function buildSummary(
  spec: FormSpec,
  state: Record<string, FieldState>,
  resolved: Record<string, Record<string, string>>,
): string {
  const lines = [`Submitted "${spec.title ?? "form"}"`];
  for (const field of spec.fields) {
    if (field.type === "hidden") continue;
    const value = state[field.name];
    if (isEmptyValue(value)) continue;
    lines.push(`- ${field.label}: ${displayValue(field, value, resolved[field.name])}`);
  }
  return lines.join("\n");
}

/** The exact machine arguments handed to the model: `field.name → value`,
 *  numbers coerced, empty optionals omitted, a single checkbox always present. */
export function buildValues(
  spec: FormSpec,
  state: Record<string, FieldState>,
): Record<string, SubmitValue> {
  const values: Record<string, SubmitValue> = {};
  for (const field of spec.fields) {
    const value = state[field.name];
    if (field.type === "checkbox" && !field.options) {
      values[field.name] = value === true;
    } else if (field.type === "number") {
      const s = String(value).trim();
      const n = Number(s);
      if (s !== "" && Number.isFinite(n)) values[field.name] = n;
    } else if (Array.isArray(value)) {
      if (value.length) values[field.name] = value;
    } else if (value !== "") {
      values[field.name] = value;
    }
  }
  return values;
}

// ─── wf-diagram ────────────────────────────────────────────────────────────
// The agent emits structure only — nodes, edges, participants — never geometry.
// Placement is the renderer's job (see diagramLayout), so a model that cannot
// reason about pixels still produces a readable diagram. Validation normalizes
// every optional to a concrete value so the layout and drawing code never has to
// re-apply a default. Ids are checked for SHAPE here; resolving an edge endpoint
// against the node set belongs to layout, which reports what it could not place.

export type DiagramShape = "box" | "round" | "diamond" | "cylinder";
export type DiagramTone = "default" | "accent" | "good" | "warn" | "bad";
export type DiagramStyle = "clean" | "sketch";
export type DiagramDirection = "down" | "right";
export type EdgeStyle = "solid" | "dashed";
export type EdgeArrow = "to" | "both" | "none";

const SHAPES: readonly DiagramShape[] = ["box", "round", "diamond", "cylinder"];
const TONES: readonly DiagramTone[] = ["default", "accent", "good", "warn", "bad"];
const STYLES: readonly DiagramStyle[] = ["clean", "sketch"];
const DIRECTIONS: readonly DiagramDirection[] = ["down", "right"];
const EDGE_STYLES: readonly EdgeStyle[] = ["solid", "dashed"];
const EDGE_ARROWS: readonly EdgeArrow[] = ["to", "both", "none"];

export interface DiagramNode {
  id: string;
  label: string;
  shape: DiagramShape;
  tone: DiagramTone;
  group?: string;
  /** `[col, row]` placement hint; honoured only when it keeps every incident
   *  edge rank-increasing (see diagramLayout). Never pixels. */
  at?: [number, number];
}

export interface DiagramEdge {
  from: string;
  to: string;
  label?: string;
  style: EdgeStyle;
  arrow: EdgeArrow;
}

export interface FlowSpec {
  kind: "flow";
  nodes: DiagramNode[];
  edges: DiagramEdge[];
  title?: string;
  direction: DiagramDirection;
  style: DiagramStyle;
}

export interface SequenceParticipant {
  id: string;
  label: string;
}

export interface SequenceStep {
  from: string;
  to: string;
  label?: string;
  style: EdgeStyle;
  note?: string;
}

export interface SequenceSpec {
  kind: "sequence";
  participants: SequenceParticipant[];
  steps: SequenceStep[];
  title?: string;
  style: DiagramStyle;
}

export type DiagramSpec = FlowSpec | SequenceSpec;

export const MAX_NODES = 60;
export const MAX_EDGES = 120;
export const MAX_PARTICIPANTS = 12;
export const MAX_STEPS = 60;
export const MAX_LABEL = 80;

const oneOf = <T extends string>(v: unknown, allowed: readonly T[], fallback: T): T =>
  typeof v === "string" && (allowed as readonly string[]).includes(v) ? (v as T) : fallback;

/** A trimmed, length-capped label; `null` when there is no usable text. An
 *  over-long label is truncated rather than rejected — the diagram is still
 *  worth drawing. */
function toLabel(v: unknown): string | null {
  if (typeof v !== "string") return null;
  const trimmed = v.trim();
  if (!trimmed) return null;
  return trimmed.length > MAX_LABEL ? trimmed.slice(0, MAX_LABEL - 1) + "…" : trimmed;
}

const toId = (v: unknown): string => (typeof v === "string" ? v.trim() : "");

function toGridHint(v: unknown): [number, number] | undefined {
  if (!Array.isArray(v) || v.length !== 2) return undefined;
  const [col, row] = v;
  if (!Number.isInteger(col) || !Number.isInteger(row) || col < 0 || row < 0) return undefined;
  return [col, row];
}

function validateFlow(value: Record<string, unknown>): Validated<FlowSpec> {
  if (!Array.isArray(value.nodes) || value.nodes.length === 0) {
    return { ok: false, error: "nodes must be a non-empty array" };
  }
  if (value.nodes.length > MAX_NODES) {
    return { ok: false, error: `too many nodes (max ${MAX_NODES})` };
  }
  const nodes: DiagramNode[] = [];
  const ids = new Set<string>();
  for (const [i, n] of value.nodes.entries()) {
    if (!isRecord(n)) return { ok: false, error: `node ${i + 1} must be an object` };
    const id = toId(n.id);
    if (!id) return { ok: false, error: `node ${i + 1} needs an id` };
    const label = toLabel(n.label);
    if (!label) return { ok: false, error: `node "${id}" needs a label` };
    // Merging a duplicate would silently draw a different process than the one
    // described, so it fails the whole spec.
    if (ids.has(id)) return { ok: false, error: `duplicate node id "${id}"` };
    ids.add(id);
    nodes.push({
      id,
      label,
      shape: oneOf(n.shape, SHAPES, "box"),
      tone: oneOf(n.tone, TONES, "default"),
      group: toLabel(n.group) ?? undefined,
      at: toGridHint(n.at),
    });
  }
  const raw = value.edges ?? [];
  if (!Array.isArray(raw)) return { ok: false, error: "edges must be an array" };
  if (raw.length > MAX_EDGES) return { ok: false, error: `too many edges (max ${MAX_EDGES})` };
  const edges: DiagramEdge[] = [];
  for (const [i, e] of raw.entries()) {
    if (!isRecord(e)) return { ok: false, error: `edge ${i + 1} must be an object` };
    const from = toId(e.from);
    const to = toId(e.to);
    if (!from || !to) return { ok: false, error: `edge ${i + 1} needs from and to` };
    edges.push({
      from,
      to,
      label: toLabel(e.label) ?? undefined,
      style: oneOf(e.style, EDGE_STYLES, "solid"),
      arrow: oneOf(e.arrow, EDGE_ARROWS, "to"),
    });
  }
  return {
    ok: true,
    spec: {
      kind: "flow",
      nodes,
      edges,
      title: toLabel(value.title) ?? undefined,
      direction: oneOf(value.direction, DIRECTIONS, "down"),
      style: oneOf(value.style, STYLES, "clean"),
    },
  };
}

function validateSequence(value: Record<string, unknown>): Validated<SequenceSpec> {
  if (!Array.isArray(value.participants) || value.participants.length < 2) {
    return { ok: false, error: "a sequence needs at least 2 participants" };
  }
  if (value.participants.length > MAX_PARTICIPANTS) {
    return { ok: false, error: `too many participants (max ${MAX_PARTICIPANTS})` };
  }
  const participants: SequenceParticipant[] = [];
  const ids = new Set<string>();
  for (const [i, p] of value.participants.entries()) {
    if (!isRecord(p)) return { ok: false, error: `participant ${i + 1} must be an object` };
    const id = toId(p.id);
    if (!id) return { ok: false, error: `participant ${i + 1} needs an id` };
    if (ids.has(id)) return { ok: false, error: `duplicate participant id "${id}"` };
    ids.add(id);
    participants.push({ id, label: toLabel(p.label) ?? id });
  }
  if (!Array.isArray(value.steps) || value.steps.length === 0) {
    return { ok: false, error: "steps must be a non-empty array" };
  }
  if (value.steps.length > MAX_STEPS) {
    return { ok: false, error: `too many steps (max ${MAX_STEPS})` };
  }
  const steps: SequenceStep[] = [];
  for (const [i, s] of value.steps.entries()) {
    if (!isRecord(s)) return { ok: false, error: `step ${i + 1} must be an object` };
    const from = toId(s.from);
    const to = toId(s.to);
    if (!from || !to) return { ok: false, error: `step ${i + 1} needs from and to` };
    steps.push({
      from,
      to,
      label: toLabel(s.label) ?? undefined,
      style: oneOf(s.style, EDGE_STYLES, "solid"),
      note: toLabel(s.note) ?? undefined,
    });
  }
  return {
    ok: true,
    spec: {
      kind: "sequence",
      participants,
      steps,
      title: toLabel(value.title) ?? undefined,
      style: oneOf(value.style, STYLES, "clean"),
    },
  };
}

export function validateDiagram(value: unknown): Validated<DiagramSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  if (value.kind === "flow") return validateFlow(value);
  if (value.kind === "sequence") return validateSequence(value);
  return { ok: false, error: 'kind must be "flow" or "sequence"' };
}

/** One picture the agent asked to show. `artifactId` is a REQUEST to render,
 *  never a grant: the host fetches it through its own authenticated,
 *  owner-scoped file route, so an id the model invented or
 *  lifted from somewhere else simply 404s. */
export interface ImageRef {
  artifactId: string;
  caption?: string;
  alt?: string;
}

export interface ImageSpec {
  images: ImageRef[];
}

/** Ceiling on one block. A model asked for "the photos for this item" can name
 *  a whole catalogue; past this many the message is a scroll of pictures rather
 *  than an answer, and each one is a separate authenticated fetch. */
export const MAX_IMAGES = 12;

function toImageRef(v: unknown): ImageRef | null {
  if (!isRecord(v)) return null;
  const { artifact_id, caption, alt } = v;
  // The id shape is checked here so a malformed one degrades to the code block
  // rather than becoming a request for a resource that cannot exist.
  if (typeof artifact_id !== "string" || !ARTIFACT_ID.test(artifact_id)) return null;
  return {
    artifactId: artifact_id,
    caption: optionalString(caption),
    alt: optionalString(alt),
  };
}

/** `{"images":[…]}` — always a list, even for one picture. A singular form
 *  would give the model two names for one concept, and several images in one
 *  block is the common case ("show me the photos for this item"). */
export function validateImage(value: unknown): Validated<ImageSpec> {
  if (!isRecord(value)) return { ok: false, error: "payload must be a JSON object" };
  if (!Array.isArray(value.images)) return { ok: false, error: "images must be an array" };
  const images = value.images
    .slice(0, MAX_IMAGES)
    .map(toImageRef)
    .filter((r): r is ImageRef => r !== null);
  // Every entry unusable means the model named nothing real — show the payload
  // rather than an empty frame that looks like a rendering failure.
  if (images.length === 0) {
    return { ok: false, error: "no image carried a valid artifact_id" };
  }
  return { ok: true, spec: { images } };
}
