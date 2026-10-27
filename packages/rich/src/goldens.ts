// The shared golden corpus: the specs whose layout both implementations must
// agree on, byte for byte.
//
// The TypeScript engine is the reference — it is the shipped one — so the
// fixtures under fixtures/diagrams/ are GENERATED from it (`npm run goldens`)
// and the Rust port is written against them. Invariants alone would not do this
// job: two layouts can each be internally correct, satisfy every invariant, and
// still draw different pictures for the same spec.
//
// Metrics are fixed here so both sides lay out at identical text sizes; nothing
// about the real font matters to the comparison.

import type { LayoutMetrics } from "./diagramLayout";

export const GOLDEN_METRICS: LayoutMetrics = { charWidth: 7, lineHeight: 16 };

export const GOLDEN_CASES: { name: string; spec: Record<string, unknown> }[] = [
  {
    name: "flow-pipeline",
    spec: {
      kind: "flow",
      title: "Order pipeline",
      nodes: [
        { id: "in", label: "Order received", shape: "round" },
        { id: "chk", label: "Stock available?", shape: "diamond" },
        { id: "pick", label: "Pick and pack", group: "Warehouse" },
        { id: "bo", label: "Backorder", group: "Warehouse", tone: "warn" },
        { id: "inv", label: "Raise invoice", group: "Finance" },
        { id: "done", label: "Notify customer", shape: "round" },
      ],
      edges: [
        { from: "in", to: "chk" },
        { from: "chk", to: "pick", label: "yes" },
        { from: "chk", to: "bo", label: "no" },
        { from: "pick", to: "inv" },
        { from: "inv", to: "done" },
        { from: "bo", to: "done" },
      ],
    },
  },
  {
    name: "flow-cycle",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "Draft" },
        { id: "b", label: "Review" },
        { id: "c", label: "Publish" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
        { from: "c", to: "a", label: "rework" },
      ],
    },
  },
  {
    name: "flow-spanning-edge",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "One" },
        { id: "b", label: "Two" },
        { id: "c", label: "Three" },
        { id: "d", label: "Four" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
        { from: "c", to: "d" },
        { from: "a", to: "d", label: "skip" },
      ],
    },
  },
  {
    name: "flow-hint-honoured",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "Alpha" },
        { id: "b", label: "Beta" },
        { id: "c", label: "Gamma", at: [0, 3] },
      ],
      edges: [{ from: "a", to: "b" }],
    },
  },
  {
    name: "flow-hint-ignored",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "Alpha" },
        { id: "b", label: "Beta", at: [0, 0] },
      ],
      edges: [{ from: "a", to: "b" }],
    },
  },
  {
    name: "flow-dropped-endpoint",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "Alpha" },
        { id: "b", label: "Beta" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "a", to: "ghost" },
      ],
    },
  },
  {
    name: "flow-lane-cap",
    spec: {
      kind: "flow",
      nodes: ["One", "Two", "Three", "Four", "Five", "Six", "Seven"].map((g, i) => ({
        id: `n${i}`,
        label: `${g} team`,
        group: g,
      })),
      edges: [0, 1, 2, 3, 4, 5].map((i) => ({ from: `n${i}`, to: `n${i + 1}` })),
    },
  },
  {
    name: "flow-right",
    spec: {
      kind: "flow",
      direction: "right",
      nodes: [
        { id: "a", label: "Intake", shape: "round" },
        { id: "b", label: "Assess", shape: "diamond" },
        { id: "c", label: "Store", shape: "cylinder" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c", style: "dashed", arrow: "both" },
      ],
    },
  },
  {
    name: "flow-self-edge",
    spec: {
      kind: "flow",
      nodes: [
        { id: "a", label: "Poll" },
        { id: "b", label: "Done" },
      ],
      edges: [
        { from: "a", to: "a", label: "retry" },
        { from: "a", to: "b" },
      ],
    },
  },
  {
    name: "sequence-refund",
    spec: {
      kind: "sequence",
      title: "Refund request",
      participants: [
        { id: "c", label: "Customer" },
        { id: "a", label: "Agent" },
        { id: "p", label: "Payments" },
      ],
      steps: [
        { from: "c", to: "a", label: "Requests refund" },
        { from: "a", to: "p", label: "Check eligibility" },
        { from: "p", to: "a", label: "Eligible", style: "dashed" },
        { from: "a", to: "a", label: "Records note", note: "Logged against the account for audit" },
        { from: "a", to: "c", label: "Refund confirmed" },
        { from: "a", to: "nobody" },
      ],
    },
  },
];

// The chart corpus pins validation, not drawing: the Rust renderer draws its
// own SVG, but which specs it accepts, how it normalises them and how it
// rejects the rest must match `validateChart` exactly — error text included.
export const CHART_GOLDEN_CASES: { name: string; input: unknown }[] = [
  {
    name: "bar-grouped",
    input: {
      kind: "bar",
      title: "Signups by plan",
      labels: ["Free", "Pro", "Team"],
      series: [
        { name: "March", data: [420, 180, 60] },
        { name: "April", data: [510, 220, 90] },
      ],
    },
  },
  {
    name: "bar-stacked-horizontal",
    input: {
      kind: "bar",
      stacked: true,
      horizontal: true,
      unit: "£",
      x_label: "Region",
      y_label: "Revenue",
      labels: ["North", "South"],
      series: [{ data: [1200.5, 800] }, { data: [300, -50] }],
    },
  },
  {
    name: "line-two-series",
    input: {
      kind: "line",
      labels: ["Jan", "Feb", "Mar"],
      series: [
        { name: "Leads", data: [12, 18, 25] },
        { name: "Won", data: [3, 5, 9] },
      ],
    },
  },
  {
    name: "area-stacked-unlabelled",
    input: { kind: "area", stacked: true, series: [{ data: [1, 2] }, { data: [2, 3, 4] }] },
  },
  {
    name: "pie-basic",
    input: {
      kind: "pie",
      title: "Ticket status",
      labels: ["Open", "Pending", "Closed"],
      series: [{ data: [14, 8, 52] }],
    },
  },
  {
    name: "pie-flags-scoped",
    input: {
      kind: "pie",
      labels: ["a"],
      series: [{ data: [1] }],
      stacked: true,
      horizontal: true,
    },
  },
  {
    name: "scatter-drops-labels",
    input: {
      kind: "scatter",
      labels: ["ignored"],
      stacked: true,
      series: [{ name: "A", data: [[2019, 5], ["2020", "7.5"]] }],
    },
  },
  {
    name: "numeric-strings-and-cell-labels",
    input: {
      kind: "bar",
      title: "  ",
      labels: [2024, 2.5, true, null, "x"],
      series: [{ data: ["42", " 7 ", "1e3", 0, -1.25] }],
    },
  },
  {
    name: "javascript-number-coercions",
    input: {
      kind: "bar",
      series: [{ name: 42, data: ["0x10", "0b11", "0o17", "+5", ".5", "5.", "1E2"] }],
    },
  },
  {
    name: "javascript-label-text",
    input: {
      kind: "line",
      labels: [1e21, 1e-7, -0, 0.1, 123456789012345680000, 1e-6, 1.5e300],
      series: [{ name: "  ", data: [1, 2, 3, 4, 5, 6, 7] }],
    },
  },
  { name: "error-boolean-data", input: { kind: "bar", series: [{ data: [true] }] } },
  { name: "error-hex-with-sign", input: { kind: "bar", series: [{ data: ["-0x10"] }] } },
  {
    name: "error-empty-labels-on-bars",
    input: { kind: "bar", labels: [], series: [{ data: [1] }] },
  },
  { name: "error-not-an-object", input: "nope" },
  { name: "error-unknown-kind", input: { kind: "donut", series: [{ data: [1] }] } },
  { name: "error-empty-series", input: { kind: "bar", series: [] } },
  { name: "error-series-shape", input: { kind: "bar", series: [{ data: 5 }] } },
  { name: "error-series-no-data", input: { kind: "bar", series: [{ data: [] }] } },
  {
    name: "error-too-many-series",
    input: { kind: "bar", series: Array.from({ length: 9 }, () => ({ data: [1] })) },
  },
  {
    name: "error-too-many-points",
    input: { kind: "line", series: [{ data: Array(1001).fill(1) }] },
  },
  { name: "error-non-numeric", input: { kind: "bar", series: [{ data: [1, "nope"] }] } },
  { name: "error-labels-null", input: { kind: "bar", labels: null, series: [{ data: [1] }] } },
  {
    name: "error-labels-not-cells",
    input: { kind: "bar", labels: [{ a: 1 }], series: [{ data: [1] }] },
  },
  {
    name: "error-pie-series-count",
    input: { kind: "pie", labels: ["a"], series: [{ data: [1] }, { data: [2] }] },
  },
  { name: "error-pie-labels-missing", input: { kind: "pie", series: [{ data: [1] }] } },
  {
    name: "error-pie-labels-mismatch",
    input: { kind: "pie", labels: ["a", "b"], series: [{ data: [1] }] },
  },
  {
    name: "error-series-labels-mismatch",
    input: { kind: "line", labels: ["a", "b"], series: [{ data: [1, 2] }, { data: [1] }] },
  },
  {
    name: "error-scatter-not-pairs",
    input: { kind: "scatter", series: [{ data: [[1, 2], [3]] }] },
  },
  {
    name: "error-scatter-not-numeric",
    input: { kind: "scatter", series: [{ data: [[1, "x"]] }] },
  },
];
