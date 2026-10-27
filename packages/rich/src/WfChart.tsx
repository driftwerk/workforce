// The wf-chart renderer: a tree-shaken echarts instance bound to a validated
// spec. Loaded lazily — this module (and echarts with it) only downloads once a
// conversation actually contains a chart.

import { useEffect, useRef } from "react";

import { initChart, type EChart } from "./echarts";
import { buildOption, chartAriaLabel } from "./chartOption";
import { readInk } from "./palette";
import { useThemeVersion } from "./theme";
import type { ChartSpec } from "./spec";

export default function WfChart({ spec }: { spec: ChartSpec }) {
  const plot = useRef<HTMLDivElement>(null);
  const chart = useRef<EChart | null>(null);
  const drawn = useRef<ChartSpec | null>(null);
  const theme = useThemeVersion();

  useEffect(() => {
    const el = plot.current;
    if (!el) return;
    const instance = initChart(el);
    chart.current = instance;
    const ro = new ResizeObserver(() => instance.resize());
    ro.observe(el);
    return () => {
      ro.disconnect();
      instance.dispose();
      chart.current = null;
    };
  }, []);

  // A new spec replaces the option outright; a theme change merges into it, so
  // only the colours move and the reader's zoom survives.
  useEffect(() => {
    const el = plot.current;
    if (!el || !chart.current) return;
    const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const fresh = drawn.current !== spec;
    drawn.current = spec;
    chart.current.setOption(buildOption(spec, readInk(el), reducedMotion), { notMerge: fresh });
  }, [spec, theme]);

  return (
    <div className="wf-chart">
      {spec.title && <div className="wf-chart-title">{spec.title}</div>}
      <div ref={plot} className="wf-chart-plot" role="img" aria-label={chartAriaLabel(spec)} />
    </div>
  );
}
