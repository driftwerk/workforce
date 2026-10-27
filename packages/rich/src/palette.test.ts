// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import css from "./styles.css?raw";

import { CATEGORICAL, INK_TOKENS, readInk, SEQUENTIAL } from "./palette";

const lightBlock = css.slice(css.indexOf(":where(:root)"), css.indexOf(':where(:root[data-theme="dark"])'));

// The stylesheet declares the defaults hosts see; the TS arrays are what a
// chart falls back to when no stylesheet is loaded. Nothing else keeps the two
// the same palette.
describe("stylesheet and chart fallbacks agree", () => {
  it("declares the categorical palette in slot order", () => {
    for (const [i, hue] of CATEGORICAL.entries()) {
      expect(lightBlock).toContain(`--rich-series-${i + 1}: ${hue};`);
    }
    expect(lightBlock).not.toContain(`--rich-series-${CATEGORICAL.length + 1}:`);
  });

  it("declares the sequential ramp low to high", () => {
    for (const [i, hue] of SEQUENTIAL.entries()) {
      expect(lightBlock).toContain(`--rich-seq-${i + 1}: ${hue};`);
    }
    expect(lightBlock).not.toContain(`--rich-seq-${SEQUENTIAL.length + 1}:`);
  });

  it("declares every token a chart reads", () => {
    for (const token of INK_TOKENS) expect(lightBlock).toContain(`${token}:`);
  });
});

describe("readInk", () => {
  afterEach(() => {
    document.body.innerHTML = "";
    document.documentElement.removeAttribute("style");
  });

  it("falls back to the default palette when no token is set", () => {
    const ink = readInk(document.body.appendChild(document.createElement("div")));
    expect(ink.series).toEqual(CATEGORICAL);
    expect(ink.sequential).toEqual(SEQUENTIAL);
    expect(ink.surface).toBe("#ffffff");
  });

  it("reads the host's tokens, inherited from any ancestor", () => {
    document.documentElement.style.setProperty("--rich-text", "#123456");
    document.documentElement.style.setProperty("--rich-series-2", "rgb(1, 2, 3)");
    document.documentElement.style.setProperty("--rich-seq-7", "#000000");
    const ink = readInk(document.body.appendChild(document.createElement("div")));
    expect(ink.text).toBe("#123456");
    expect(ink.series[1]).toBe("rgb(1, 2, 3)");
    expect(ink.series[0]).toBe(CATEGORICAL[0]);
    expect(ink.sequential[6]).toBe("#000000");
  });

  // A host's own :root tokens must survive a dark theme switched on below it:
  // the package's dark defaults may not land on a closer ancestor.
  it("keeps the host's root tokens under a nested dark theme", () => {
    const sheet = document.createElement("style");
    sheet.textContent = `${css}\n:root { --rich-bg: #111210; }`;
    document.head.append(sheet);
    const container = document.body.appendChild(document.createElement("div"));
    container.setAttribute("data-theme", "dark");
    const ink = readInk(container.appendChild(document.createElement("div")));
    sheet.remove();
    expect(ink.surface).toBe("#111210");
  });
});
