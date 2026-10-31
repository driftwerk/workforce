import { describe, expect, it } from "vitest";

import { layoutDiagram } from "./diagramLayout";
import { CHART_GOLDEN_CASES, GOLDEN_CASES, GOLDEN_METRICS } from "./goldens";
import { validateChart, validateDiagram } from "./spec";

// The corpora the Rust ports are written against. Asserting them here too means
// a change to this engine fails on THIS side first, with a readable diff,
// instead of surfacing as an unexplained mismatch in the Rust suite.
function corpus<T>(modules: Record<string, { default: T }>): Map<string, T> {
  return new Map(
    Object.entries(modules).map(([path, mod]) => [path.split("/").pop()!.replace(".json", ""), mod.default]),
  );
}

const diagrams = corpus(
  import.meta.glob("../../../fixtures/diagrams/*.json", { eager: true }) as Record<
    string,
    { default: { spec: unknown; metrics: unknown; layout: unknown } }
  >,
);

const charts = corpus(
  import.meta.glob("../../../fixtures/charts/*.json", { eager: true }) as Record<
    string,
    { default: { input: unknown; result: unknown } }
  >,
);

describe("golden layouts", () => {
  it("has a committed fixture for every case", () => {
    expect([...diagrams.keys()].sort()).toEqual(GOLDEN_CASES.map((c) => c.name).sort());
  });

  for (const { name, spec } of GOLDEN_CASES) {
    it(`${name} still lays out exactly as committed`, () => {
      const golden = diagrams.get(name);
      expect(golden, `run \`npm run goldens\` to create ${name}.json`).toBeDefined();
      // The Rust suite reads the file's spec, so an edit here must reach it.
      expect(golden!.spec).toStrictEqual(spec);
      const validated = validateDiagram(spec);
      if (!validated.ok) throw new Error(validated.error);
      expect(layoutDiagram(validated.spec, GOLDEN_METRICS)).toEqual(golden!.layout);
      // Both sides must lay out at the same text size or nothing else compares.
      expect(golden!.metrics).toEqual({
        char_width: GOLDEN_METRICS.charWidth,
        line_height: GOLDEN_METRICS.lineHeight,
      });
    });
  }
});

describe("golden chart validation", () => {
  it("has a committed fixture for every case", () => {
    expect([...charts.keys()].sort()).toEqual(CHART_GOLDEN_CASES.map((c) => c.name).sort());
  });

  for (const { name, input } of CHART_GOLDEN_CASES) {
    it(`${name} still validates exactly as committed`, () => {
      const golden = charts.get(name);
      expect(golden, `run \`npm run goldens\` to create ${name}.json`).toBeDefined();
      // Through JSON so `undefined` optionals drop out and `-0` reads back as
      // `0`, as they do in the file the Rust suite reads.
      const roundTrip = (v: unknown) => JSON.parse(JSON.stringify(v));
      expect(golden!.input).toStrictEqual(roundTrip(input));
      expect(roundTrip(validateChart(input))).toStrictEqual(golden!.result);
    });
  }

  // The hub stages a chart's spec as it normalised it — exactly the golden
  // `spec` — and a reader draws that file through this validator again, so a
  // normalised spec must come back unchanged.
  for (const [name, golden] of charts) {
    const result = golden.result as { ok: boolean; spec?: unknown };
    if (!result.ok) continue;
    it(`${name}'s normalised spec, as the hub stages it, validates unchanged`, () => {
      const staged = JSON.parse(JSON.stringify(result.spec));
      expect(JSON.parse(JSON.stringify(validateChart(staged)))).toStrictEqual({ ok: true, spec: result.spec });
    });
  }
});
