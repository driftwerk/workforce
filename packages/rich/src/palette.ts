// Chart color roles, read from the host's `--rich-*` tokens when a chart draws.
// Series identity uses a validated categorical palette (fixed slot order — the
// ordering is the CVD-safety mechanism, never cycle or re-derive it); chart
// chrome reads the neutral tokens so axes and labels follow the host's theme.
// The arrays here are the defaults styles.css declares and the fallbacks when a
// token is unset; a test holds the two to each other.

export const CATEGORICAL: readonly string[] = [
  "#2a78d6", // blue
  "#1baf7a", // aqua
  "#eda100", // yellow
  "#008300", // green
  "#4a3aa7", // violet
  "#e34948", // red
  "#e87ba4", // magenta
  "#eb6834", // orange
];

// Magnitude uses a validated single-hue sequential ramp (blue, light→dark) — the
// choropleth encoding, never the categorical slots. Given low→high to a
// continuous visualMap, which interpolates between the stops.
export const SEQUENTIAL: readonly string[] = [
  "#cde2fb",
  "#9ec5f4",
  "#6da7ec",
  "#3987e5",
  "#256abf",
  "#184f95",
  "#0d366b",
];

// Diagram lane hues, light mode. Colour on a diagram encodes ONE thing — which
// group owns a step — so this is a categorical identity palette, assigned by the
// group's first appearance and never cycled.
//
// Five, not eight: the chart palette's 7th↔8th pair sits at ΔE 12.9 (below the
// 15 normal-vision floor), and green had to go because darkening it for a dark
// ground collapses yellow↔green to ΔE 0.8 under protanopia. Both mode sets are
// validated by the dataviz palette checker; the DARK steps are SELECTED, not
// derived — the dark lightness band (0.48–0.67) is narrower and lower than
// light's (0.43–0.77), so blue, yellow and violet are all different values.
//
// The renderer paints from the `--rich-lane-*` tokens (so a theme switch
// costs no re-render); this list is the same palette for the standalone
// Excalidraw export, and a test asserts the two never drift apart.
export const LANE_HUES: readonly string[] = [
  "#2a78d6", // blue
  "#1baf7a", // aqua
  "#eda100", // yellow
  "#4a3aa7", // violet
  "#e34948", // red
];

export interface ChartInk {
  text: string;
  muted: string;
  grid: string;
  axis: string;
  surface: string;
  font: string;
  series: readonly string[];
  sequential: readonly string[];
}

const FALLBACK: ChartInk = {
  text: "#0f172a",
  muted: "#64748b",
  grid: "#e6eaf0",
  axis: "#e6eaf0",
  surface: "#ffffff",
  font: 'system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
  series: CATEGORICAL,
  sequential: SEQUENTIAL,
};

/** The ground a block draws on when the host sets no `--rich-bg`. */
export const FALLBACK_BG = FALLBACK.surface;

/** Every token a chart or map bakes into its option — the set whose change
 *  must trigger a redraw. */
export const INK_TOKENS: readonly string[] = [
  "--rich-text",
  "--rich-muted",
  "--rich-border",
  "--rich-bg",
  ...CATEGORICAL.map((_, i) => `--rich-series-${i + 1}`),
  ...SEQUENTIAL.map((_, i) => `--rich-seq-${i + 1}`),
];

const SRGB_SYNTAX = /^(#|rgba?\(|hsla?\()/i;
let probe: CanvasRenderingContext2D | null | undefined;

/** ECharts parses colours itself and knows only hex, rgb and hsl, so a token in
 *  any other CSS colour syntax (oklch, color-mix, …) is resolved to sRGB by
 *  painting one pixel with it. */
function concrete(color: string): string {
  if (SRGB_SYNTAX.test(color)) return color;
  if (probe === undefined) {
    probe = document.createElement("canvas").getContext("2d", { willReadFrequently: true });
  }
  if (!probe) return color;
  probe.clearRect(0, 0, 1, 1);
  probe.fillStyle = "#000";
  probe.fillStyle = color;
  probe.fillRect(0, 0, 1, 1);
  const [r, g, b, a] = probe.getImageData(0, 0, 1, 1).data;
  return a === 255 ? `rgb(${r}, ${g}, ${b})` : `rgba(${r}, ${g}, ${b}, ${(a / 255).toFixed(3)})`;
}

/** Resolve the chart's colours from the element's computed theme tokens. */
export function readInk(el: HTMLElement): ChartInk {
  const style = getComputedStyle(el);
  const token = (name: string, fallback: string) =>
    concrete(style.getPropertyValue(name).trim() || fallback);
  const ramp = (prefix: string, fallback: readonly string[]) =>
    fallback.map((hue, i) => token(`${prefix}${i + 1}`, hue));
  return {
    text: token("--rich-text", FALLBACK.text),
    muted: token("--rich-muted", FALLBACK.muted),
    grid: token("--rich-border", FALLBACK.grid),
    axis: token("--rich-border", FALLBACK.axis),
    surface: token("--rich-bg", FALLBACK.surface),
    font: style.fontFamily || FALLBACK.font,
    series: ramp("--rich-series-", FALLBACK.series),
    sequential: ramp("--rich-seq-", FALLBACK.sequential),
  };
}
