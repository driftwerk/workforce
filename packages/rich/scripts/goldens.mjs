// Write the shared golden corpora from the TypeScript engine — the reference
// implementation. The Rust ports are tested against these files, so
// regenerating them is a deliberate act: a diff here is a change in what both
// sides draw (diagrams) or accept (charts).
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { layoutDiagram } from "../src/diagramLayout.ts";
import { validateChart, validateDiagram } from "../src/spec.ts";
import { CHART_GOLDEN_CASES, GOLDEN_CASES, GOLDEN_METRICS } from "../src/goldens.ts";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../../../fixtures");

function write(dir, name, body) {
  mkdirSync(join(fixtures, dir), { recursive: true });
  writeFileSync(join(fixtures, dir, `${name}.json`), JSON.stringify(body, null, 2) + "\n");
  console.log(`${dir}/${name}.json`);
}

for (const { name, spec } of GOLDEN_CASES) {
  const validated = validateDiagram(spec);
  if (!validated.ok) throw new Error(`${name}: ${validated.error}`);
  write("diagrams", name, {
    spec,
    metrics: { char_width: GOLDEN_METRICS.charWidth, line_height: GOLDEN_METRICS.lineHeight },
    layout: layoutDiagram(validated.spec, GOLDEN_METRICS),
  });
}

for (const { name, input } of CHART_GOLDEN_CASES) {
  write("charts", name, { input, result: validateChart(input) });
}
