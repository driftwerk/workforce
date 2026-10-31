// The wf-chart renderer: a tree-shaken echarts instance bound to a validated
// spec. Loaded lazily — this module (and echarts with it) only downloads once a
// conversation actually contains a chart. Under the plot the reader can see the
// numbers as a table, and copy or save the chart as a picture of what is on
// screen, in the host's theme.

import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";

import { initChart, type EChart } from "./echarts";
import { buildOption, chartAriaLabel } from "./chartOption";
import { chartTable } from "./chartTable";
import { useSaveFile } from "./host";
import { readInk } from "./palette";
import { rasterize, standaloneSvg } from "./svgExport";
import { retryImport } from "./lazyRetry";
import { useThemeVersion } from "./theme";
import type { ChartSpec } from "./spec";

// The table is its own chunk, the one the table block loads: most readers
// never ask for a chart's data.
const WfTable = lazy(() => retryImport(() => import("./WfTable")));

const slug = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "") || "chart";

type Said = "copied" | "copy-failed" | "save-failed" | null;

const SAID: Record<Exclude<Said, null>, string> = {
  copied: "Copied",
  "copy-failed": "The image couldn’t be copied.",
  "save-failed": "The image couldn’t be created.",
};

/** The chart as drawn, as a PNG on the theme's ground. */
async function picture(plot: HTMLElement): Promise<Blob> {
  const svg = plot.querySelector("svg");
  const { clientWidth: width, clientHeight: height } = plot;
  if (!svg || !width || !height) throw new Error("the chart is not drawn");
  return rasterize(standaloneSvg(svg, width, height), width, height);
}

export default function WfChart({ spec }: { spec: ChartSpec }) {
  const plot = useRef<HTMLDivElement>(null);
  const chart = useRef<EChart | null>(null);
  const drawn = useRef<ChartSpec | null>(null);
  const theme = useThemeVersion();
  const save = useSaveFile();
  const [asTable, setAsTable] = useState(false);
  const [said, setSaid] = useState<Said>(null);
  const table = useMemo(() => (asTable ? chartTable(spec) : null), [asTable, spec]);

  useEffect(() => {
    const el = plot.current;
    if (!el) return;
    const instance = initChart(el);
    chart.current = instance;
    // Hidden behind the table it has no size; it is drawn again when shown.
    const ro = new ResizeObserver(() => {
      if (el.clientWidth && el.clientHeight) instance.resize();
    });
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

  useEffect(() => {
    if (said !== "copied") return;
    const timer = window.setTimeout(() => setSaid(null), 1600);
    return () => window.clearTimeout(timer);
  }, [said]);

  const name = `${slug(spec.title ?? "chart")}.png`;

  const saveImage = async () => {
    const el = plot.current;
    if (!el) return;
    setSaid(null);
    try {
      save(await picture(el), name);
    } catch {
      setSaid("save-failed");
    }
  };

  // The clipboard is written within the click, with the picture to follow:
  // WebKit refuses a write that starts after an await.
  const copyImage = () => {
    const el = plot.current;
    const clipboard = typeof navigator === "undefined" ? undefined : navigator.clipboard;
    if (!el || !clipboard?.write || typeof ClipboardItem === "undefined") {
      setSaid("copy-failed");
      return;
    }
    clipboard.write([new ClipboardItem({ "image/png": picture(el) })]).then(
      () => setSaid("copied"),
      () => setSaid("copy-failed"),
    );
  };

  return (
    <div className="wf-chart">
      {spec.title && <div className="wf-chart-title">{spec.title}</div>}
      <div
        ref={plot}
        className="wf-chart-plot"
        role="img"
        aria-label={chartAriaLabel(spec)}
        hidden={asTable}
      />
      {table && (
        <Suspense fallback={<div className="wf-skel wf-skel-table" role="status" />}>
          <WfTable spec={table} />
        </Suspense>
      )}
      <div className="wf-chart-actions">
        <button
          type="button"
          className={"wf-rt-btn" + (asTable ? " wf-rt-on" : "")}
          aria-pressed={asTable}
          onClick={() => setAsTable((on) => !on)}
        >
          {asTable ? "Show chart" : "Show data"}
        </button>
        <button type="button" className="wf-rt-btn" disabled={asTable} onClick={copyImage}>
          Copy image
        </button>
        <button type="button" className="wf-rt-btn" disabled={asTable} onClick={() => void saveImage()}>
          Save image
        </button>
        {said && (
          <span className="wf-chart-said" role={said === "copied" ? "status" : "alert"}>
            {SAID[said]}
          </span>
        )}
      </div>
    </div>
  );
}
