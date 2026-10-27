# Geography data — sources, licences, attribution

Each `*.json` here is a normalized GeoJSON `FeatureCollection` whose every feature
carries a single `name` property (the canonical region key the agent emits) and a
simplified geometry. Regenerate with `node scripts/build-geo.mjs`. Boundaries are
**approximate**, sized for a chat-block choropleth, not authoritative.

The `attribution` string in `registry.ts` is rendered under the rendered map. Keep
it in sync with this file.

## uk-postcode-areas.json

- **What:** the 120 Great Britain postcode areas (the leading letters of a postcode
  — `W`, `SW`, `EC`, `M`, `LS`, …), dissolved up from postcode-district polygons.
- **Source:** [missinglink/uk-postcode-polygons](https://github.com/missinglink/uk-postcode-polygons)
  (postcode-district polygons transcribed from Wikipedia).
- **Licence:** **CC BY-SA 3.0.** This derived file is likewise offered under
  CC BY-SA 3.0.
- **Attribution (required):** “UK postcode boundaries © Wikipedia contributors,
  [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/).”

## world-countries.json

- **What:** 177 countries at 1:110m.
- **Source:** [Natural Earth](https://www.naturalearthdata.com/) via
  [nvkelso/natural-earth-vector](https://github.com/nvkelso/natural-earth-vector).
- **Licence:** **public domain** (no attribution required).
- **Attribution (courtesy):** “Made with Natural Earth.”
