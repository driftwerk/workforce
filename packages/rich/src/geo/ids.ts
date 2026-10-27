// The geography ids a wf-map may reference. Kept free of the `?url` asset
// imports so spec validation can gate `geo` without pulling the boundary files
// into every consumer's module graph.

export const GEOGRAPHY_IDS = ["uk-postcode-areas", "world-countries"] as const;

export type GeographyId = (typeof GEOGRAPHY_IDS)[number];

export const isGeographyId = (s: string): s is GeographyId =>
  (GEOGRAPHY_IDS as readonly string[]).includes(s);
