/**
 * How the board sizes its columns and words a column's window. The hub
 * resolves and applies windows; the board only checks a viewer's stored
 * choice and says what the hub sent.
 */

import type { PipelineDefinition } from "./definition";

export const DEFAULT_CARDS = 10;
export const MAX_CARDS = 50;

/**
 * A card's height in rem, from its classes: `py-2` and `border`, the
 * single-line `text-xs` id row, two `leading-snug` summary lines, the
 * single-line `text-[10px]` footer row and the two `gap-1` between them. An
 * annotation chip is not counted, so a card carrying one takes a little more.
 */
export const CARD_REM = 5.5208;
/** The rung chips (`text-[10px]` with a border) and the `gap-1` above them. */
export const RUNG_ROW_REM = 1.2083;
/** The `gap-2` between cards. */
export const CARD_GAP_REM = 0.5;
/** A column's border, its header (`py-2`, a `text-xs` line, `border-b`) and its body's `p-2`. */
export const COLUMN_CHROME_REM = 3.1875;
/** A window footer: `border-t` and `py-1.5` round a `py-0.5` trigger on a 10px line at the page's 1.5 line height. */
export const WINDOW_FOOTER_REM = 2;

/**
 * Every column's height in rem: room for `cards` routine cards, each with a
 * rung row when the definition has `rungs`, plus a window footer's room when
 * any column has a window, so every column is the same height.
 */
export function columnHeight(cards: number, rungs: boolean, footer: boolean): number {
  const slot = CARD_REM + (rungs ? RUNG_ROW_REM : 0);
  return COLUMN_CHROME_REM + (footer ? WINDOW_FOOTER_REM : 0) + cards * slot + (cards - 1) * CARD_GAP_REM;
}

export function isCards(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= MAX_CARDS;
}

/** The choices a viewer has for cards per column: 5, 10, 20 and the definition's own. */
export function cardChoices(defined: number): number[] {
  return [...new Set([5, 10, 20, defined])].sort((a, b) => a - b);
}

/** A viewer's own choice, else the definition's, else the hub's default. */
export function cardsPerColumn(def: PipelineDefinition, override?: number): number {
  return override ?? def.board?.cards ?? DEFAULT_CARDS;
}

export interface ParsedWindow {
  text: string;
  hours: number;
}

const MIN_HOURS = 24;
const MAX_HOURS = 3650 * 24;
const SPAN = /^([1-9]\d*)([hd])$/;

/** A window as the hub reads it: `<n>h` or `<n>d` from 24 hours to 3650 days, or `all`; null for anything else. */
export function parseWindow(text: string): ParsedWindow | null {
  if (text === "all") return { text, hours: Infinity };
  const match = SPAN.exec(text);
  if (!match) return null;
  const [, count, unit] = match;
  const hours = Number(count) * (unit === "d" ? 24 : 1);
  return hours >= MIN_HOURS && hours <= MAX_HOURS ? { text, hours } : null;
}

const PRESETS: ParsedWindow[] = [
  { text: "24h", hours: 24 },
  { text: "7d", hours: 168 },
  { text: "30d", hours: 720 },
  { text: "all", hours: Infinity },
];

/** A windowed column's choices, shortest first: 24h, 7d, 30d and all, with the definition's window in its own spelling. */
export function windowChoices(defaultWindow: string): string[] {
  const defined = parseWindow(defaultWindow);
  const choices = defined ? [...PRESETS.filter((preset) => preset.hours !== defined.hours), defined] : PRESETS;
  return [...choices].sort((a, b) => (a.hours === b.hours ? 0 : a.hours < b.hours ? -1 : 1)).map((choice) => choice.text);
}

/** The `windows` the board asks the hub for: `all` in replay, else the viewer's choices by column; undefined for the definition's. */
export function windowsParam(overrides: Record<string, string>, replay: boolean): string | undefined {
  if (replay) return "all";
  const entries = Object.entries(overrides)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([column, window]) => `${column}:${window}`);
  return entries.length > 0 ? entries.join(",") : undefined;
}

/** Whether any lane or exit keeps only what entered it recently. */
export function hasWindows(def: PipelineDefinition): boolean {
  return [...def.lanes, ...(def.exits ?? [])].some((column) => column.window !== undefined);
}

/** A window in words: "24 hours", "7 days", "1 day", "all time". */
export function windowLabel(window: string): string {
  if (window === "all") return "all time";
  const match = SPAN.exec(window);
  if (!match) return window;
  const [, count, unit] = match;
  return `${count} ${unit === "h" ? "hour" : "day"}${count === "1" ? "" : "s"}`;
}

/** How far back a window reaches, to follow "shows" or "from": "the last 7 days", "all time". */
export function windowReach(window: string): string {
  return window === "all" ? "all time" : `the last ${windowLabel(window)}`;
}

/** What a windowed column's footer says of the items its window leaves out. */
export function hiddenText(hidden: number, window: string): string {
  if (hidden > 0) return `${hidden} older hidden`;
  return window === "all" ? "all shown" : "nothing older";
}

/** What an empty column says, naming its window when that hides older items. */
export function emptyText(window: { window: string; hidden: number } | undefined): string {
  return window && window.hidden > 0 ? `None in ${windowReach(window.window)}` : "None";
}
