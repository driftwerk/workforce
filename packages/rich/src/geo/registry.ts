// The boundary sources a wf-map can render. Each geography is a normalized
// GeoJSON file (every feature's sole property is its canonical `name`) loaded by
// a dynamic import, so each one is its own chunk, fetched only when a map of
// that geography first renders — by whatever bundler the host uses, with no
// asset URL to serve. Provenance, licences and attribution live in ./NOTICE.md.

import { retryImport } from "../lazyRetry";
import { GEOGRAPHY_IDS, type GeographyId } from "./ids";

export interface GeoFeature {
  type: "Feature";
  properties: { name: string };
  geometry: { type: string; coordinates: unknown };
}

export interface GeoFeatureCollection {
  type: "FeatureCollection";
  features: GeoFeature[];
}

export interface Geography {
  load: () => Promise<{ default: unknown }>;
  label: string;
  attribution?: string;
  /** Lowercased region-name variants the model may emit, mapped to the canonical
   *  feature name — the net for spellings that differ from the source data. */
  aliases?: Record<string, string>;
}

export const GEOGRAPHIES: Record<GeographyId, Geography> = {
  "uk-postcode-areas": {
    load: () => import("./uk-postcode-areas.json"),
    label: "UK postcode areas",
    attribution: "UK postcode boundaries © Wikipedia contributors, CC BY-SA 3.0",
  },
  "world-countries": {
    load: () => import("./world-countries.json"),
    label: "World countries",
    attribution: "Made with Natural Earth",
    aliases: {
      "united states": "United States of America",
      usa: "United States of America",
      us: "United States of America",
      uk: "United Kingdom",
      "great britain": "United Kingdom",
      britain: "United Kingdom",
      "czech republic": "Czechia",
      "ivory coast": "Côte d'Ivoire",
      "democratic republic of the congo": "Dem. Rep. Congo",
      "dr congo": "Dem. Rep. Congo",
      "republic of the congo": "Congo",
      "bosnia and herzegovina": "Bosnia and Herz.",
      "central african republic": "Central African Rep.",
      "dominican republic": "Dominican Rep.",
      "equatorial guinea": "Eq. Guinea",
      "south sudan": "S. Sudan",
      "western sahara": "W. Sahara",
      macedonia: "North Macedonia",
      burma: "Myanmar",
      swaziland: "eSwatini",
      "east timor": "Timor-Leste",
    },
  },
};

const cache = new Map<GeographyId, Promise<GeoFeatureCollection>>();

/** Load a geography's boundaries, retried on transient failure and memoized so
 *  a second map of the same geography reuses the first load. */
export function loadGeography(id: GeographyId): Promise<GeoFeatureCollection> {
  const hit = cache.get(id);
  if (hit) return hit;
  const load = retryImport(GEOGRAPHIES[id].load)
    .then((module) => module.default as GeoFeatureCollection)
    .catch((err) => {
      cache.delete(id);
      throw err;
    });
  cache.set(id, load);
  return load;
}

export { GEOGRAPHY_IDS };
