// Regenerates the normalized geography files under src/geo/.
// Run manually when a boundary source changes: `node scripts/build-geo.mjs`.
// Requires network + npx (downloads mapshaper on first run). The committed JSON
// outputs are what the package build consumes — this script is provenance only.
//
// Every output feature is normalized to a single `name` property holding the
// canonical region key the model emits, geometry simplified to a tolerance
// legible at chat-block size. See src/geo/NOTICE.md for sources, licences,
// attribution.

import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = fileURLToPath(new URL("../src/geo/", import.meta.url));
const work = mkdtempSync(join(tmpdir(), "wf-geo-"));
const mapshaper = (args) =>
  execFileSync("npx", ["-y", "mapshaper@0.6", ...args], { stdio: "inherit" });

async function fetchJson(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status} ${url}`);
  return r.json();
}

// UK postcode areas: dissolve missinglink's per-district polygons up to the
// postcode-area prefix (the file name), tagging each district with its area.
async function ukPostcodeAreas() {
  const base =
    "https://raw.githubusercontent.com/missinglink/uk-postcode-polygons/master/geojson/";
  const index = await fetchJson(
    "https://api.github.com/repos/missinglink/uk-postcode-polygons/contents/geojson",
  );
  const codes = index
    .map((e) => e.name.replace(/\.geojson$/, ""))
    .sort();
  const features = [];
  for (const code of codes) {
    const fc = await fetchJson(base + code + ".geojson");
    for (const f of fc.features ?? [])
      features.push({ type: "Feature", properties: { name: code }, geometry: f.geometry });
  }
  const src = join(work, "uk-districts.geojson");
  writeFileSync(src, JSON.stringify({ type: "FeatureCollection", features }));
  mapshaper([
    src,
    "-rename-fields", "pc=name",
    "-dissolve", "pc",
    "-simplify", "3%", "keep-shapes",
    "-rename-fields", "name=pc",
    "-filter-fields", "name",
    "-o", join(OUT, "uk-postcode-areas.json"), "format=geojson",
  ]);
}

// World countries: Natural Earth 1:110m, name = English country name.
async function worldCountries() {
  const src = join(work, "world.geojson");
  const fc = await fetchJson(
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_110m_admin_0_countries.geojson",
  );
  writeFileSync(src, JSON.stringify(fc));
  mapshaper([
    src,
    "-rename-fields", "name=NAME",
    "-filter-fields", "name",
    "-simplify", "12%", "keep-shapes",
    "-o", join(OUT, "world-countries.json"), "format=geojson",
  ]);
}

try {
  await ukPostcodeAreas();
  await worldCountries();
} finally {
  rmSync(work, { recursive: true, force: true });
}
