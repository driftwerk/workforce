// Charts and maps bake their colours into an ECharts option, so unlike the
// CSS-painted blocks they must be told when the theme changes. One observer,
// started with the first chart on screen and stopped with the last, watches the
// places hosts switch themes — class, style and data-theme on <html> and
// <body>, and the OS colour scheme — and bumps a version only when something a
// chart reads actually changed, so unrelated style writes (a scroll lock) never
// redraw.

import { useSyncExternalStore } from "react";

import { INK_TOKENS } from "./palette";

const WATCHED = ["class", "style", "data-theme"];

let version = 0;
let signature = "";
let stop: (() => void) | null = null;
const listeners = new Set<() => void>();

function snapshot(): string {
  const root = document.documentElement;
  const body = document.body;
  const style = getComputedStyle(body ?? root);
  return [
    root.className,
    root.getAttribute("data-theme"),
    body?.className,
    body?.getAttribute("data-theme"),
    ...INK_TOKENS.map((t) => style.getPropertyValue(t)),
  ].join("|");
}

function bump() {
  version++;
  for (const listener of listeners) listener();
}

function check() {
  const next = snapshot();
  if (next === signature) return;
  signature = next;
  bump();
}

/** Redraw every chart and map from the current tokens. Call it after a theme
 *  change the renderer cannot observe: a swapped stylesheet, or a class toggled
 *  on an element other than <html> or <body>. */
export function refreshRichTheme(): void {
  if (listeners.size) signature = snapshot();
  bump();
}

function start(): () => void {
  signature = snapshot();
  const observer = new MutationObserver(check);
  const options = { attributes: true, attributeFilter: WATCHED };
  observer.observe(document.documentElement, options);
  if (document.body) observer.observe(document.body, options);
  const scheme =
    typeof window.matchMedia === "function" ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  scheme?.addEventListener?.("change", refreshRichTheme);
  return () => {
    observer.disconnect();
    scheme?.removeEventListener?.("change", refreshRichTheme);
  };
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  stop ??= start();
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && stop) {
      stop();
      stop = null;
    }
  };
}

const current = () => version;

/** A number that changes whenever the theme does — an effect dependency for
 *  anything that resolves tokens into concrete colours. */
export function useThemeVersion(): number {
  return useSyncExternalStore(subscribe, current, current);
}
