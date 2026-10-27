import { describe, expect, it } from "vitest";

import {
  buildSummary,
  buildValues,
  cellValue,
  fieldError,
  formValid,
  inferColumnTypes,
  initialFormState,
  MAX_EDGES,
  MAX_FORM_FIELDS,
  MAX_LABEL,
  MAX_NODES,
  MAX_OPTIONS,
  MAX_PARTICIPANTS,
  MAX_POINTS,
  MAX_REGIONS,
  MAX_ROWS,
  MAX_SERIES,
  MAX_STEPS,
  optionsFromPage,
  parsePayload,
  validateChart,
  validateDiagram,
  validateForm,
  validateMap,
  validateTable,
  type Cell,
  type FieldState,
  type FlowSpec,
  type FormField,
  type FormSpec,
  type InlineTableSpec,
  type SequenceSpec,
  type TablePage,
} from "./spec";

/** Validate to an inline table, asserting the mode so tests can read columns/rows. */
function inline(value: unknown): { spec: InlineTableSpec; truncated?: string } {
  const v = validateTable(value);
  if (!v.ok) throw new Error(v.error);
  if (v.spec.mode !== "inline") throw new Error("expected an inline table");
  return { spec: v.spec, truncated: v.truncated };
}

describe("parsePayload", () => {
  it("parses strict JSON", () => {
    expect(parsePayload('{"a":1}')).toEqual({ a: 1 });
  });

  it("recovers from trailing commas outside strings", () => {
    expect(parsePayload('{"a":[1,2,],}')).toEqual({ a: [1, 2] });
    expect(parsePayload('{"a":"keep,}","b":1}')).toEqual({ a: "keep,}", b: 1 });
  });

  it("returns null for garbage and truncated JSON", () => {
    expect(parsePayload("not json")).toBeNull();
    expect(parsePayload('{"columns":["A"],"rows":[["x"')).toBeNull();
    expect(parsePayload("")).toBeNull();
  });
});

describe("validateTable", () => {
  const valid = { columns: ["Name", "Salary"], rows: [["Ann", 100], ["Bob", 90]] };

  it("accepts string columns and normalizes them", () => {
    const v = inline(valid);
    expect(v.spec.mode).toBe("inline");
    expect(v.spec.columns).toEqual([
      { key: "Name", label: "Name" },
      { key: "Salary", label: "Salary" },
    ]);
    expect(v.spec.rows).toHaveLength(2);
    expect(v.truncated).toBeUndefined();
  });

  it("detects an artifact-backed table by its artifact_id", () => {
    const id = "art_0123456789abcdef0123456789abcdef";
    const v = validateTable({ title: "Recent leads", artifact_id: id });
    if (!v.ok) throw new Error(v.error);
    expect(v.spec).toEqual({ mode: "artifact", title: "Recent leads", artifactId: id });
    // A malformed id falls through to inline validation.
    expect(inline({ artifact_id: "art_short", columns: ["A"], rows: [[1]] }).spec.mode).toBe("inline");
  });

  it("keeps an artifact table's column overlay and rejects a malformed one", () => {
    const id = "art_0123456789abcdef0123456789abcdef";
    const v = validateTable({
      artifact_id: id,
      columns: [{ key: "total_fee", label: "Fee", format: "currency", unit: "£" }],
    });
    if (!v.ok) throw new Error(v.error);
    expect(v.spec).toEqual({
      mode: "artifact",
      artifactId: id,
      columns: [{ key: "total_fee", label: "Fee", format: "currency", unit: "£" }],
    });
    expect(validateTable({ artifact_id: id, columns: [{ label: "no key" }] }).ok).toBe(false);
  });

  it("accepts object columns with label, align, format and unit", () => {
    const v = inline({ columns: [{ key: "amt", label: "Amount", align: "right" }], rows: [[5]] });
    expect(v.spec.columns[0]).toEqual({ key: "amt", label: "Amount", align: "right" });
    const money = inline({
      columns: [{ key: "fee", format: "currency", unit: " £ " }, { key: "ref", format: "bogus" }],
      rows: [[5, 1]],
    });
    expect(money.spec.columns[0]).toEqual({ key: "fee", label: "fee", format: "currency", unit: "£" });
    // An unknown format is dropped rather than failing the whole table.
    expect(money.spec.columns[1]).toEqual({ key: "ref", label: "ref" });
  });

  it("normalizes ragged rows to the column count", () => {
    const v = inline({ columns: ["A", "B"], rows: [["only"], ["x", "y", "extra"]] });
    expect(v.spec.rows[0]).toEqual(["only", null]);
    expect(v.spec.rows[1]).toEqual(["x", "y"]);
  });

  it("rejects non-scalar cells, bad columns, and bad shapes", () => {
    expect(validateTable({ columns: ["A"], rows: [[{ nested: 1 }]] }).ok).toBe(false);
    expect(validateTable({ columns: [], rows: [] }).ok).toBe(false);
    expect(validateTable({ columns: [{ label: "no key" }], rows: [] }).ok).toBe(false);
    expect(validateTable({ columns: ["A"], rows: ["not-an-array"] }).ok).toBe(false);
    expect(validateTable([1, 2]).ok).toBe(false);
  });

  it("caps rows with a truncation notice", () => {
    const rows = Array.from({ length: MAX_ROWS + 5 }, (_, i) => [i]);
    const v = inline({ columns: ["N"], rows });
    expect(v.spec.rows).toHaveLength(MAX_ROWS);
    expect(v.truncated).toContain("2,000");
  });
});

describe("validateChart", () => {
  it("accepts every kind", () => {
    for (const kind of ["bar", "line", "area"]) {
      const v = validateChart({ kind, labels: ["a", "b"], series: [{ data: [1, 2] }] });
      expect(v.ok, kind).toBe(true);
    }
    expect(
      validateChart({ kind: "pie", labels: ["a", "b"], series: [{ data: [1, 2] }] }).ok,
    ).toBe(true);
    expect(
      validateChart({ kind: "scatter", series: [{ data: [[1, 2], [3, 4]] }] }).ok,
    ).toBe(true);
  });

  it("coerces numeric strings and rejects non-numeric data", () => {
    const v = validateChart({ kind: "bar", series: [{ data: ["42", 7] }] });
    if (!v.ok) throw new Error(v.error);
    expect(v.spec.series[0].data).toEqual([42, 7]);
    expect(validateChart({ kind: "bar", series: [{ data: ["nope"] }] }).ok).toBe(false);
    expect(validateChart({ kind: "bar", series: [{ data: [null] }] }).ok).toBe(false);
  });

  it("enforces pie constraints", () => {
    expect(validateChart({ kind: "pie", series: [{ data: [1] }] }).ok).toBe(false);
    expect(
      validateChart({ kind: "pie", labels: ["a"], series: [{ data: [1] }, { data: [2] }] }).ok,
    ).toBe(false);
    expect(
      validateChart({ kind: "pie", labels: ["a", "b"], series: [{ data: [1] }] }).ok,
    ).toBe(false);
  });

  it("requires series to match labels when labels are present", () => {
    expect(
      validateChart({ kind: "line", labels: ["a", "b"], series: [{ data: [1] }] }).ok,
    ).toBe(false);
  });

  it("enforces caps and scatter pair shape", () => {
    const many = Array.from({ length: MAX_SERIES + 1 }, () => ({ data: [1] }));
    expect(validateChart({ kind: "bar", series: many }).ok).toBe(false);
    expect(
      validateChart({ kind: "bar", series: [{ data: Array(MAX_POINTS + 1).fill(1) }] }).ok,
    ).toBe(false);
    expect(validateChart({ kind: "scatter", series: [{ data: [1, 2] }] }).ok).toBe(false);
    expect(validateChart({ kind: "scatter", series: [{ data: [[1]] }] }).ok).toBe(false);
  });

  it("scopes stacked and horizontal to the kinds that support them", () => {
    const stackedPie = validateChart({
      kind: "pie",
      labels: ["a"],
      series: [{ data: [1] }],
      stacked: true,
      horizontal: true,
    });
    if (!stackedPie.ok) throw new Error(stackedPie.error);
    expect(stackedPie.spec.stacked).toBe(false);
    expect(stackedPie.spec.horizontal).toBe(false);

    const hbar = validateChart({ kind: "bar", series: [{ data: [1] }], horizontal: true });
    if (!hbar.ok) throw new Error(hbar.error);
    expect(hbar.spec.horizontal).toBe(true);
  });

  it("rejects unknown kinds and empty series", () => {
    expect(validateChart({ kind: "donut", series: [{ data: [1] }] }).ok).toBe(false);
    expect(validateChart({ kind: "bar", series: [] }).ok).toBe(false);
    expect(validateChart({ kind: "bar", series: [{ data: [] }] }).ok).toBe(false);
  });
});

describe("validateMap", () => {
  const good = {
    kind: "choropleth",
    geo: "uk-postcode-areas",
    regions: [
      { name: "W", value: 489 },
      { name: "SW", value: 451 },
    ],
  };

  it("accepts a well-formed choropleth and keeps optional fields", () => {
    const v = validateMap({ ...good, title: "Leads", value_label: "Leads", unit: "£", min: 0, max: 500 });
    expect(v.ok).toBe(true);
    if (!v.ok) return;
    expect(v.spec.geo).toBe("uk-postcode-areas");
    expect(v.spec.regions).toEqual(good.regions);
    expect(v.spec.unit).toBe("£");
    expect(v.spec.min).toBe(0);
    expect(v.spec.max).toBe(500);
  });

  it("coerces numeric-string values and trims names", () => {
    const v = validateMap({ ...good, regions: [{ name: "  W ", value: "489" }] });
    expect(v.ok).toBe(true);
    if (!v.ok) return;
    expect(v.spec.regions[0]).toEqual({ name: "W", value: 489 });
  });

  it("rejects a wrong kind, unknown geography, and empty regions", () => {
    expect(validateMap({ ...good, kind: "bubble" }).ok).toBe(false);
    expect(validateMap({ ...good, geo: "mars" }).ok).toBe(false);
    expect(validateMap({ ...good, regions: [] }).ok).toBe(false);
    expect(validateMap("nope").ok).toBe(false);
  });

  it("rejects non-scalar region shapes and over-long region lists", () => {
    expect(validateMap({ ...good, regions: [{ name: "W" }] }).ok).toBe(false);
    expect(validateMap({ ...good, regions: [{ name: "W", value: "x" }] }).ok).toBe(false);
    expect(validateMap({ ...good, regions: [{ value: 1 }] }).ok).toBe(false);
    const many = Array.from({ length: MAX_REGIONS + 1 }, () => ({ name: "W", value: 1 }));
    expect(validateMap({ ...good, regions: many }).ok).toBe(false);
  });
});

describe("inferColumnTypes / cellValue", () => {
  const spec = (rows: Cell[][]): InlineTableSpec => ({
    mode: "inline",
    columns: rows[0].map((_, i) => ({ key: `c${i}`, label: `c${i}` })),
    rows,
  });

  it("infers number, date, and string columns", () => {
    const t = inferColumnTypes(
      spec([
        [1, "2026-01-02", "mixed"],
        ["2.5", "2026-03-04T10:00", 7],
        [null, null, "text"],
      ]),
    );
    expect(t).toEqual(["number", "date", "string"]);
  });

  it("falls back to string for all-null columns and non-uniform values", () => {
    expect(inferColumnTypes(spec([[null], [""]]))).toEqual(["string"]);
    expect(inferColumnTypes(spec([[1], ["x"]]))).toEqual(["string"]);
  });

  it("cellValue produces sortable values with nulls as null", () => {
    expect(cellValue("2.5", "number")).toBe(2.5);
    expect(cellValue(null, "number")).toBeNull();
    expect(cellValue("", "date")).toBeNull();
    expect(cellValue("2026-01-02", "date")).toBe(Date.parse("2026-01-02"));
    expect(cellValue(true, "string")).toBe("true");
  });
});

/** Validate a form spec, throwing on degrade so a test can read the fields. */
function form(value: unknown): FormSpec {
  const v = validateForm(value);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
}

/** One normalised field from a single-field spec, for `fieldError` cases. */
function fieldOf(raw: Record<string, unknown>): FormField {
  return form({ fields: [raw] }).fields[0];
}

const ARTIFACT = "art_0123456789abcdef0123456789abcdef";

describe("validateForm", () => {
  it("accepts a full spec and normalises every field", () => {
    const spec = form({
      title: "New lead",
      submit_label: "Create lead",
      note: "We'll create this in the CRM.",
      fields: [
        { name: "company", label: "Company", type: "text", required: true, max_length: 120 },
        { name: "email", label: "Email", type: "email", required: true },
        { name: "value", label: "Deal value", type: "number", min: 0 },
        { name: "close", label: "Close", type: "date" },
        { name: "status", type: "select", required: true, options: [{ value: "open", label: "Open" }, { value: "won" }, "lost"], value: "open" },
        { name: "owner", type: "select", required: true, options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } },
        { name: "channels", label: "Channels", type: "checkbox", options: [{ value: "email" }, { value: "phone" }] },
        { name: "priority", type: "radio", options: [{ value: "low" }, { value: "high" }], value: "low" },
        { name: "notes", type: "textarea", max_length: 2000 },
        { name: "source", type: "hidden", value: 42 },
      ],
    });
    expect(spec.title).toBe("New lead");
    expect(spec.submitLabel).toBe("Create lead");
    expect(spec.note).toBe("We'll create this in the CRM.");
    expect(spec.fields).toHaveLength(10);

    const [company, , , , status, owner, channels, priority, , source] = spec.fields;
    expect(company).toMatchObject({ label: "Company", required: true, maxLength: 120 });
    // A bare string and a value-only object both normalise; `label` defaults to `value`.
    expect(status.options).toEqual([
      { value: "open", label: "Open" },
      { value: "won", label: "won" },
      { value: "lost", label: "lost" },
    ]);
    expect(owner.options).toBeUndefined();
    expect(owner.optionsRef).toEqual({ artifactId: ARTIFACT, value: "id", label: "name" });
    expect(channels).toMatchObject({ multiple: true });
    expect(priority.label).toBe("priority"); // label defaults to name
    expect(source).toMatchObject({ type: "hidden", value: "42" });
  });

  it("defaults submit label, keeps a good pattern, drops a bad one", () => {
    expect(form({ fields: [{ name: "a", type: "text" }] }).submitLabel).toBe("Submit");
    expect(fieldOf({ name: "a", type: "text", pattern: "^[a-z]+$" }).pattern).toBe("^[a-z]+$");
    expect(fieldOf({ name: "a", type: "text", pattern: "[" }).pattern).toBeUndefined();
  });

  it("distinguishes single vs multi selection", () => {
    expect(fieldOf({ name: "s", type: "select", options: [{ value: "a" }], multiple: true }).multiple).toBe(true);
    // options_ref selects are always single (the typeahead picks one row).
    expect(fieldOf({ name: "s", type: "select", options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" }, multiple: true }).multiple).toBeUndefined();
    // A checkbox with no options is a single boolean; with options it is multi.
    expect(fieldOf({ name: "c", type: "checkbox" }).multiple).toBeUndefined();
    expect(fieldOf({ name: "c", type: "checkbox", options: [{ value: "x" }] }).multiple).toBe(true);
  });

  it("rejects every malformed shape", () => {
    expect(validateForm([1, 2]).ok).toBe(false);
    expect(validateForm({ fields: [] }).ok).toBe(false);
    expect(validateForm({ fields: "no" }).ok).toBe(false);
    expect(validateForm({ fields: [{ type: "text" }] }).ok).toBe(false); // no name
    expect(validateForm({ fields: [{ name: "a", type: "wat" }] }).ok).toBe(false); // bad type
    expect(validateForm({ fields: [{ name: "a", type: "text" }, { name: "a", type: "email" }] }).ok).toBe(false); // dup
    expect(validateForm({ fields: [{ name: "s", type: "select" }] }).ok).toBe(false); // no options/ref
    expect(validateForm({ fields: [{ name: "r", type: "radio" }] }).ok).toBe(false); // radio needs options
    expect(validateForm({ fields: [{ name: "s", type: "select", options: [{ label: "no value" }] }] }).ok).toBe(false); // bad option
    expect(validateForm({ fields: [{ name: "s", type: "select", options: "no" }] }).ok).toBe(false); // options not array
    expect(validateForm({ fields: [{ name: "h", type: "hidden" }] }).ok).toBe(false); // hidden needs value
  });

  it("enforces the field and option caps", () => {
    const fields = Array.from({ length: MAX_FORM_FIELDS + 1 }, (_, i) => ({ name: `f${i}`, type: "text" }));
    expect(validateForm({ fields }).ok).toBe(false);
    const options = Array.from({ length: MAX_OPTIONS + 1 }, (_, i) => ({ value: `o${i}` }));
    expect(validateForm({ fields: [{ name: "s", type: "select", options }] }).ok).toBe(false);
  });
});

describe("fieldError", () => {
  it("gates required — empty string and empty array fail, false is valid", () => {
    expect(fieldError(fieldOf({ name: "a", type: "text", required: true }), "")).toBe("Required");
    expect(fieldError(fieldOf({ name: "a", type: "text", required: true }), "  ")).toBe("Required");
    expect(fieldError(fieldOf({ name: "a", type: "text", required: true }), "ok")).toBeNull();
    expect(fieldError(fieldOf({ name: "c", type: "checkbox", options: [{ value: "x" }], required: true }), [])).toBe("Required");
    // A single (boolean) required checkbox is satisfied even when false.
    expect(fieldError(fieldOf({ name: "c", type: "checkbox", required: true }), false)).toBeNull();
    // An empty, non-required field passes without further checks.
    expect(fieldError(fieldOf({ name: "a", type: "email" }), "")).toBeNull();
  });

  it("checks text length and pattern", () => {
    const f = fieldOf({ name: "a", type: "text", min_length: 2, max_length: 4 });
    expect(fieldError(f, "x")).toContain("at least 2");
    expect(fieldError(f, "toolong")).toContain("at most 4");
    expect(fieldError(f, "abc")).toBeNull();
    const p = fieldOf({ name: "a", type: "text", pattern: "^[0-9]+$" });
    expect(fieldError(p, "12a")).toBe("Invalid format");
    expect(fieldError(p, "123")).toBeNull();
  });

  it("checks email, number range, and date range", () => {
    expect(fieldError(fieldOf({ name: "e", type: "email" }), "nope")).toBe("Enter a valid email address");
    expect(fieldError(fieldOf({ name: "e", type: "email" }), "a@b.co")).toBeNull();
    const n = fieldOf({ name: "n", type: "number", min: 0, max: 10 });
    expect(fieldError(n, "abc")).toBe("Enter a number");
    expect(fieldError(n, "-1")).toContain("at least 0");
    expect(fieldError(n, "11")).toContain("at most 10");
    expect(fieldError(n, "5")).toBeNull();
    const d = fieldOf({ name: "d", type: "date", min: "2026-01-01", max: "2026-12-31" });
    expect(fieldError(d, "not-a-date")).toBe("Enter a valid date");
    expect(fieldError(d, "2025-06-01")).toContain("on or after");
    expect(fieldError(d, "2027-01-01")).toContain("on or before");
    expect(fieldError(d, "2026-06-01")).toBeNull();
  });

  it("checks enum membership and multi-value arrays, but not options_ref", () => {
    const sel = fieldOf({ name: "s", type: "select", options: [{ value: "a" }, { value: "b" }] });
    expect(fieldError(sel, "z")).toBe("Select a valid option");
    expect(fieldError(sel, "a")).toBeNull();
    const grp = fieldOf({ name: "c", type: "checkbox", options: [{ value: "a" }, { value: "b" }] });
    expect(fieldError(grp, ["a", "z"])).toBe("Select a valid option");
    expect(fieldError(grp, ["a", "b"])).toBeNull();
    // options_ref: any chosen row value is accepted (the full set is server-side).
    const ref = fieldOf({ name: "o", type: "select", options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } });
    expect(fieldError(ref, "anything_the_typeahead_returned")).toBeNull();
  });
});

describe("form state + submission helpers", () => {
  const spec = form({
    title: "Lead",
    fields: [
      { name: "company", label: "Company", type: "text", required: true },
      { name: "status", label: "Status", type: "select", options: [{ value: "open", label: "Open" }], value: "open" },
      { name: "channels", label: "Channels", type: "checkbox", options: [{ value: "email", label: "Email" }, { value: "phone", label: "Phone" }] },
      { name: "subscribed", label: "Subscribed", type: "checkbox" },
      { name: "value", label: "Deal value", type: "number" },
      { name: "owner", label: "Owner", type: "select", options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } },
      { name: "source", type: "hidden", value: "web" },
    ],
  });

  it("initialises state from prefills and control kinds", () => {
    const state = initialFormState(spec);
    expect(state).toMatchObject({
      company: "",
      status: "open",
      channels: [],
      subscribed: false,
      value: "",
      owner: "",
      source: "web",
    });
  });

  it("blocks Submit until required fields are valid", () => {
    const state = initialFormState(spec);
    expect(formValid(spec, state)).toBe(false);
    expect(formValid(spec, { ...state, company: "Acme" })).toBe(true);
  });

  it("builds the human summary from labels and chosen option labels", () => {
    const state: Record<string, FieldState> = {
      company: "Acme",
      status: "open",
      channels: ["email", "phone"],
      subscribed: true,
      value: "500",
      owner: "u_7",
      source: "web",
    };
    const resolved = { owner: { u_7: "Dana Scully" } };
    const summary = buildSummary(spec, state, resolved);
    expect(summary).toBe(
      [
        'Submitted "Lead"',
        "- Company: Acme",
        "- Status: Open",
        "- Channels: Email, Phone",
        "- Subscribed: Yes",
        "- Deal value: 500",
        "- Owner: Dana Scully", // resolved label, not the raw id
      ].join("\n"),
    );
    // Hidden fields never appear in the readable summary.
    expect(summary).not.toContain("web");
  });

  it("builds the exact machine values, coercing numbers and omitting empties", () => {
    const state: Record<string, FieldState> = {
      company: "Acme",
      status: "",
      channels: [],
      subscribed: false,
      value: "500",
      owner: "u_7",
      source: "web",
    };
    expect(buildValues(spec, state)).toEqual({
      company: "Acme",
      subscribed: false, // a single checkbox is always present
      value: 500, // coerced to a number
      owner: "u_7",
      source: "web", // hidden carried through
      // status "" and empty channels omitted
    });
  });
});

describe("optionsFromPage", () => {
  const page = (columns: [string, string][], rows: Cell[][]): TablePage => ({
    columns: columns.map(([key, label]) => ({ key, label, type: "text" })),
    rows,
    total: rows.length,
  });

  it("maps rows using the ref's value/label columns", () => {
    const p = page(
      [["id", "Id"], ["name", "Name"]],
      [["u_1", "Ann"], ["u_2", "Bob"]],
    );
    expect(optionsFromPage(p, { artifactId: ARTIFACT, value: "id", label: "name" })).toEqual([
      { value: "u_1", label: "Ann" },
      { value: "u_2", label: "Bob" },
    ]);
  });

  it("falls back to the value when the label column is missing, and drops null values", () => {
    const p = page([["id", "Id"]], [["u_1"], [null]]);
    expect(optionsFromPage(p, { artifactId: ARTIFACT, value: "id", label: "name" })).toEqual([
      { value: "u_1", label: "u_1" },
    ]);
    // A missing value column yields no options.
    expect(optionsFromPage(p, { artifactId: ARTIFACT, value: "missing", label: "name" })).toEqual([]);
  });
});

describe("validateDiagram", () => {
  const flow = (payload: Record<string, unknown>) => validateDiagram({ kind: "flow", ...payload });
  const nodes = [
    { id: "a", label: "Start" },
    { id: "b", label: "Finish" },
  ];

  it("accepts a flow and normalizes every optional to a concrete value", () => {
    const result = flow({ nodes, edges: [{ from: "a", to: "b" }] });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const spec = result.spec as FlowSpec;
    expect(spec.direction).toBe("down");
    expect(spec.style).toBe("clean");
    expect(spec.nodes[0]).toEqual({ id: "a", label: "Start", shape: "box", tone: "default", group: undefined, at: undefined });
    expect(spec.edges[0]).toEqual({ from: "a", to: "b", label: undefined, style: "solid", arrow: "to" });
  });

  it("accepts a sequence", () => {
    const result = validateDiagram({
      kind: "sequence",
      participants: [{ id: "c", label: "Customer" }, { id: "a", label: "Agent" }],
      steps: [{ from: "c", to: "a", label: "Asks", style: "dashed" }],
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const spec = result.spec as SequenceSpec;
    expect(spec.participants).toHaveLength(2);
    expect(spec.steps[0].style).toBe("dashed");
  });

  it("rejects a payload that is not a known diagram", () => {
    expect(validateDiagram("nope").ok).toBe(false);
    expect(validateDiagram({ kind: "mindmap", nodes }).ok).toBe(false);
    expect(flow({ nodes: [] }).ok).toBe(false);
    expect(flow({ nodes: [{ id: "a" }] }).ok).toBe(false);
    expect(flow({ nodes: [{ label: "no id" }] }).ok).toBe(false);
  });

  it("fails a duplicate node id rather than merging it", () => {
    const result = flow({ nodes: [{ id: "a", label: "One" }, { id: "a", label: "Two" }] });
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toContain("duplicate");
  });

  it("accepts an edge naming no declared node — resolution belongs to layout", () => {
    const result = flow({ nodes, edges: [{ from: "a", to: "ghost" }] });
    expect(result.ok).toBe(true);
    if (result.ok) expect((result.spec as FlowSpec).edges).toHaveLength(1);
  });

  it("falls back on unknown enum values instead of failing", () => {
    const result = flow({
      nodes: [{ id: "a", label: "Start", shape: "hexagon", tone: "neon" }],
      edges: [{ from: "a", to: "a", style: "wiggly", arrow: "sideways" }],
      direction: "sideways",
      style: "watercolour",
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const spec = result.spec as FlowSpec;
    expect(spec.nodes[0].shape).toBe("box");
    expect(spec.nodes[0].tone).toBe("default");
    expect(spec.edges[0].style).toBe("solid");
    expect(spec.edges[0].arrow).toBe("to");
    expect(spec.direction).toBe("down");
    expect(spec.style).toBe("clean");
  });

  it("keeps only a well-formed grid hint", () => {
    const hinted = flow({
      nodes: [
        { id: "a", label: "A", at: [1, 2] },
        { id: "b", label: "B", at: [0.5, 2] },
        { id: "c", label: "C", at: [-1, 0] },
        { id: "d", label: "D", at: "middle" },
      ],
    });
    expect(hinted.ok).toBe(true);
    if (!hinted.ok) return;
    const spec = hinted.spec as FlowSpec;
    expect(spec.nodes.map((n) => n.at)).toEqual([[1, 2], undefined, undefined, undefined]);
  });

  it("truncates an over-long label rather than rejecting it", () => {
    const result = flow({ nodes: [{ id: "a", label: "x".repeat(MAX_LABEL + 40) }] });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const label = (result.spec as FlowSpec).nodes[0].label;
    expect(label).toHaveLength(MAX_LABEL);
    expect(label.endsWith("…")).toBe(true);
  });

  it("refuses input past the size caps", () => {
    const many = (n: number) => Array.from({ length: n }, (_, i) => ({ id: `n${i}`, label: `N${i}` }));
    expect(flow({ nodes: many(MAX_NODES + 1) }).ok).toBe(false);
    expect(
      flow({
        nodes,
        edges: Array.from({ length: MAX_EDGES + 1 }, () => ({ from: "a", to: "b" })),
      }).ok,
    ).toBe(false);
    expect(
      validateDiagram({
        kind: "sequence",
        participants: [{ id: "a", label: "A" }],
        steps: [{ from: "a", to: "a" }],
      }).ok,
    ).toBe(false);
    expect(
      validateDiagram({
        kind: "sequence",
        participants: many(MAX_PARTICIPANTS + 1),
        steps: [{ from: "n0", to: "n1" }],
      }).ok,
    ).toBe(false);
    expect(
      validateDiagram({
        kind: "sequence",
        participants: [{ id: "a", label: "A" }, { id: "b", label: "B" }],
        steps: Array.from({ length: MAX_STEPS + 1 }, () => ({ from: "a", to: "b" })),
      }).ok,
    ).toBe(false);
  });
});
