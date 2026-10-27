// The wf-diagram renderer. Unlike the echarts-backed siblings this draws its own
// SVG, which buys two things: every label is a React text node (so agent output
// can never inject markup), and colour comes from CSS classes rather than
// resolved hex — the diagram then follows the host's light/dark and branding
// with no re-render on a theme change.

import { useLayoutEffect, useMemo, useRef, useState } from "react";

import { arrowHeads, edgePath, nodePath } from "./diagramDraw";
import {
  diagramAriaLabel,
  diagramOutline,
  layoutDiagram,
  type LaidOutLifeline,
  type LaidOutNode,
  type LaidOutNote,
  type LayoutMetrics,
} from "./diagramLayout";
import { toExcalidraw } from "./excalidraw";
import { rasterize, standaloneSvg } from "./svgExport";
import { useSaveFile } from "./host";
import type { DiagramSpec } from "./spec";

// The diagram draws at its natural size and its frame scrolls, the same answer
// `.wf-tablewrap` already gives wide tables. Fitting a whole diagram into the
// column instead would default a 12-node chain — the size the prompt asks for —
// to about half scale, putting 13px labels at 6px. Zoom is then a real choice
// (shrink for an overview, magnify for detail), not a legibility tax.
const ZOOM_STEP = 1.4;
const ZOOM_MIN = 0.4;
const ZOOM_MAX = 3;
const CHIP_PAD = 5;

// jsdom has no 2D context; the ratio matches the default system-ui stack
// closely enough that a test layout is still representative.
const FALLBACK: LayoutMetrics = { charWidth: 7.1, lineHeight: 17 };
const SAMPLE = "abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ";

const measured = new Map<string, LayoutMetrics>();
let canvas: CanvasRenderingContext2D | null | undefined;

/** Text metrics for the element's resolved font, measured once per font. */
function measure(el: HTMLElement): LayoutMetrics {
  const style = getComputedStyle(el);
  const font = `${style.fontSize} ${style.fontFamily}`;
  const cached = measured.get(font);
  if (cached) return cached;
  if (canvas === undefined) canvas = document.createElement("canvas").getContext("2d");
  if (!canvas) return FALLBACK;
  canvas.font = font;
  const metrics: LayoutMetrics = {
    charWidth: canvas.measureText(SAMPLE).width / SAMPLE.length,
    lineHeight: Math.round((parseFloat(style.fontSize) || 13) * 1.32),
  };
  measured.set(font, metrics);
  return metrics;
}

const clamp = (v: number, low: number, high: number) =>
  low > high ? (low + high) / 2 : Math.min(Math.max(v, low), high);

const slug = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "") || "diagram";

function Lines({
  lines,
  cx,
  cy,
  lineHeight,
  className,
}: {
  lines: string[];
  cx: number;
  cy: number;
  lineHeight: number;
  className: string;
}) {
  const top = cy - ((lines.length - 1) * lineHeight) / 2;
  return (
    <text className={className} textAnchor="middle" dominantBaseline="middle">
      {lines.map((line, i) => (
        <tspan key={i} x={cx} y={top + i * lineHeight}>
          {line}
        </tspan>
      ))}
    </text>
  );
}

/** A lane suffix for the CSS class, or "" when the element has no lane. */
const lane = (n?: number) => (n ? ` lane-${n}` : "");

const asNode = (
  box: LaidOutLifeline | LaidOutNote,
  id: string,
  lines: string[],
): LaidOutNode => ({
  id,
  label: lines.join(" "),
  lines,
  shape: "box",
  tone: "default",
  x: box.x,
  y: box.y,
  w: box.w,
  h: box.h,
});

export default function WfDiagram({
  spec,
  excalidraw,
}: {
  spec: DiagramSpec;
  excalidraw: boolean;
}) {
  const host = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLDivElement>(null);
  const svg = useRef<SVGSVGElement>(null);
  const [metrics, setMetrics] = useState<LayoutMetrics>(FALLBACK);
  const [zoom, setZoom] = useState(1);
  const [failed, setFailed] = useState(false);
  const save = useSaveFile();

  // Layout effect, not effect: the measured metrics land before paint, so the
  // diagram never flashes at fallback sizing and then snaps.
  useLayoutEffect(() => {
    if (host.current) setMetrics(measure(host.current));
  }, []);

  const layout = useMemo(() => layoutDiagram(spec, metrics), [spec, metrics]);
  const outline = useMemo(() => diagramOutline(layout), [layout]);

  const zoomBy = (factor: number) => setZoom((z) => clamp(z * factor, ZOOM_MIN, ZOOM_MAX));

  // Read the frame's width at click time rather than tracking it — this is the
  // only place it matters, and a diagram wider than its column is the only case
  // that wants it.
  const fitWidth = () => {
    const width = canvas.current?.clientWidth;
    if (width) setZoom(clamp(width / layout.width, ZOOM_MIN, ZOOM_MAX));
  };

  const name = slug(spec.title ?? "diagram");

  const downloadImage = async () => {
    const node = svg.current;
    if (!node) return;
    setFailed(false);
    try {
      const markup = standaloneSvg(node, layout.width, layout.height);
      save(await rasterize(markup, layout.width, layout.height), `${name}.png`);
    } catch {
      // Rasterizing goes through the browser's image decoder, so it can fail for
      // reasons we cannot inspect. Say so rather than appearing to do nothing.
      setFailed(true);
    }
  };

  const downloadScene = () => {
    const scene = toExcalidraw(layout, spec.title);
    save(
      new Blob([JSON.stringify(scene, null, 2)], { type: "application/json" }),
      `${name}.excalidraw`,
    );
  };

  return (
    <div className="wf-dg" ref={host}>
      {spec.title && <div className="wf-dg-title">{spec.title}</div>}
      <div className="wf-dg-frame">
        <div className="wf-dg-canvas" ref={canvas}>
          <svg
            ref={svg}
            className="wf-dg-svg"
            width={Math.round(layout.width * zoom)}
            height={Math.round(layout.height * zoom)}
            viewBox={`0 0 ${layout.width} ${layout.height}`}
            role="img"
            aria-label={diagramAriaLabel(spec, layout)}
          >
            {layout.groups.map((group) => (
              <g key={group.label} className={"wf-dg-group" + lane(group.lane)}>
                <rect x={group.x} y={group.y} width={group.w} height={group.h} rx={10} />
                <text x={group.x + 10} y={group.y + 13}>
                  {group.label}
                </text>
              </g>
            ))}

            {layout.lifelines.map((lifeline) => (
              <g key={lifeline.id} className={"wf-dg-lifeline" + lane(lifeline.lane)}>
                <line
                  x1={lifeline.x + lifeline.w / 2}
                  y1={lifeline.y + lifeline.h}
                  x2={lifeline.x + lifeline.w / 2}
                  y2={lifeline.bottom}
                />
                <path
                  className="wf-dg-shape"
                  d={nodePath(asNode(lifeline, lifeline.id, lifeline.lines), spec.style)}
                />
                <Lines
                  lines={lifeline.lines}
                  cx={lifeline.x + lifeline.w / 2}
                  cy={lifeline.y + lifeline.h / 2}
                  lineHeight={metrics.lineHeight}
                  className="wf-dg-text"
                />
              </g>
            ))}

            {layout.notes.map((note, i) => (
              <g key={i} className="wf-dg-note">
                <path className="wf-dg-shape" d={nodePath(asNode(note, `note~${i}`, note.lines), spec.style)} />
                <Lines
                  lines={note.lines}
                  cx={note.x + note.w / 2}
                  cy={note.y + note.h / 2}
                  lineHeight={metrics.lineHeight}
                  className="wf-dg-text"
                />
              </g>
            ))}

            {layout.edges.map((edge) => (
              <g key={edge.key} className={"wf-dg-edge style-" + edge.style}>
                <path className="wf-dg-line" d={edgePath(edge.points, edge.key, spec.style)} />
                {arrowHeads(edge.points, edge.arrow).map((d, i) => (
                  <path key={i} className="wf-dg-head" d={d} />
                ))}
                {edge.label && edge.labelAt && (
                  <>
                    <rect
                      className="wf-dg-chip"
                      x={edge.labelAt[0] - (edge.label.length * metrics.charWidth) / 2 - CHIP_PAD}
                      y={edge.labelAt[1] - metrics.lineHeight / 2}
                      width={edge.label.length * metrics.charWidth + 2 * CHIP_PAD}
                      height={metrics.lineHeight}
                      rx={4}
                    />
                    <Lines
                      lines={[edge.label]}
                      cx={edge.labelAt[0]}
                      cy={edge.labelAt[1]}
                      lineHeight={metrics.lineHeight}
                      className="wf-dg-edge-text"
                    />
                  </>
                )}
              </g>
            ))}

            {layout.nodes.map((node) => (
              <g key={node.id} className={"wf-dg-node tone-" + node.tone + lane(node.lane)}>
                <path className="wf-dg-shape" d={nodePath(node, spec.style)} />
                <Lines
                  lines={node.lines}
                  cx={node.x + node.w / 2}
                  cy={node.y + node.h / 2}
                  lineHeight={metrics.lineHeight}
                  className="wf-dg-text"
                />
              </g>
            ))}
          </svg>
        </div>

        <div className="wf-dg-zoom" role="group" aria-label="Zoom">
          <button type="button" aria-label="Zoom in" onClick={() => zoomBy(ZOOM_STEP)}>
            +
          </button>
          <button type="button" aria-label="Zoom out" onClick={() => zoomBy(1 / ZOOM_STEP)}>
            −
          </button>
          <button type="button" aria-label="Fit to width" onClick={fitWidth}>
            ⤢
          </button>
        </div>
      </div>

      {/* A shape-only aria-label cannot convey a graph, so the edges also ride
          along as sentences. */}
      <div className="wf-sr-only">
        {outline.map((step, i) => (
          <p key={i}>{step}</p>
        ))}
      </div>

      {layout.dropped.length > 0 && (
        <div className="wf-dg-note-text">
          {layout.dropped.length} {layout.dropped.length === 1 ? "connection" : "connections"}{" "}
          couldn&apos;t be drawn: {layout.dropped.join(", ")}
        </div>
      )}

      <div className="wf-dg-actions">
        <button type="button" className="wf-rt-btn" onClick={downloadImage}>
          Download image
        </button>
        {excalidraw && (
          <button type="button" className="wf-rt-btn" onClick={downloadScene}>
            Download for Excalidraw
          </button>
        )}
        {failed && (
          <span className="wf-dg-note-text" role="alert">
            The image couldn&apos;t be created.
          </span>
        )}
      </div>
    </div>
  );
}
