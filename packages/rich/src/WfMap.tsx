// The wf-map renderer: fetch the geography's boundaries, register them, and draw
// a choropleth with the tree-shaken echarts. Loaded lazily — this module (and
// echarts with it) only downloads once a conversation contains a map.

import { useEffect, useRef, useState } from "react";

import { initChart, registerGeoMap, type EChart } from "./echarts";
import { buildMapOption, mapPaintOption } from "./mapOption";
import { readInk } from "./palette";
import { useThemeVersion } from "./theme";
import { GEOGRAPHIES, loadGeography, type GeoFeatureCollection } from "./geo/registry";
import type { MapSpec } from "./spec";

const registered = new Set<string>();
const ZOOM_STEP = 1.5;
const ZOOM_MAX = 12;

export default function WfMap({ spec }: { spec: MapSpec }) {
  const geography = GEOGRAPHIES[spec.geo];
  const plot = useRef<HTMLDivElement>(null);
  const chart = useRef<EChart | null>(null);
  const zoom = useRef(1);
  const [geo, setGeo] = useState<GeoFeatureCollection | null>(null);
  const theme = useThemeVersion();
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [info, setInfo] = useState<{ unplaced: string[]; ariaLabel: string }>({
    unplaced: [],
    ariaLabel: "",
  });

  useEffect(() => {
    let live = true;
    setGeo(null);
    setFailed(false);
    loadGeography(spec.geo).then(
      (loaded) => live && setGeo(loaded),
      () => live && setFailed(true),
    );
    return () => {
      live = false;
    };
  }, [spec.geo, attempt]);

  useEffect(() => {
    const el = plot.current;
    if (!el || !geo) return;
    if (!registered.has(spec.geo)) {
      registerGeoMap(spec.geo, geo);
      registered.add(spec.geo);
    }
    const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const instance = initChart(el);
    const { option, unplaced, ariaLabel } = buildMapOption(
      spec,
      geo,
      geography.aliases ?? {},
      readInk(el),
      reducedMotion,
    );
    instance.setOption(option);
    chart.current = instance;
    zoom.current = 1;
    setInfo({ unplaced, ariaLabel });
    const ro = new ResizeObserver(() => instance.resize());
    ro.observe(el);
    return () => {
      ro.disconnect();
      instance.dispose();
      chart.current = null;
    };
  }, [spec, geo, geography]);

  // A theme change repaints the drawn map in place; rebuilding it would throw
  // away where the reader had panned and zoomed to.
  useEffect(() => {
    const el = plot.current;
    if (el && chart.current) chart.current.setOption(mapPaintOption(readInk(el)));
  }, [theme]);

  const zoomBy = (factor: number) => {
    if (!chart.current) return;
    zoom.current = Math.min(ZOOM_MAX, Math.max(1, zoom.current * factor));
    chart.current.setOption({ series: [{ zoom: zoom.current }] });
  };

  return (
    <div className="wf-map">
      {spec.title && <div className="wf-map-title">{spec.title}</div>}
      {failed ? (
        <div className="wf-map-msg" role="alert">
          The map couldn't be loaded.
          <button type="button" className="wf-rt-btn" onClick={() => setAttempt((a) => a + 1)}>
            Retry
          </button>
        </div>
      ) : !geo ? (
        <div className="wf-skel wf-skel-map" role="status">
          <span className="wf-skel-label">Loading map…</span>
        </div>
      ) : (
        <>
          <div className="wf-map-canvas">
            <div ref={plot} className="wf-map-plot" role="img" aria-label={info.ariaLabel} />
            <div className="wf-map-zoom" role="group" aria-label="Zoom">
              <button type="button" aria-label="Zoom in" onClick={() => zoomBy(ZOOM_STEP)}>
                +
              </button>
              <button type="button" aria-label="Zoom out" onClick={() => zoomBy(1 / ZOOM_STEP)}>
                −
              </button>
            </div>
          </div>
          {info.unplaced.length > 0 && (
            <div className="wf-map-note">
              {info.unplaced.length} {info.unplaced.length === 1 ? "region" : "regions"} couldn't be
              placed: {info.unplaced.join(", ")}
            </div>
          )}
          {geography.attribution && <div className="wf-map-credit">{geography.attribution}</div>}
        </>
      )}
    </div>
  );
}
