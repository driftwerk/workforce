// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import { standaloneSvg } from "./svgExport";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Build a diagram-shaped SVG whose paint comes from a stylesheet, exactly as
 *  the real renderer does — the whole point of the export is that this styling
 *  survives serialization. */
function mounted(): SVGSVGElement {
  const style = document.createElement("style");
  style.textContent = `
    .wf-dg-svg { --rich-bg: #fdfcfa; }
    .wf-dg-shape { fill: rgb(1, 2, 3); stroke: rgb(4, 5, 6); stroke-width: 1.4px; }
    .wf-dg-text { fill: rgb(7, 8, 9); font-size: 13px; }
  `;
  document.head.append(style);

  const svg = document.createElementNS(SVG_NS, "svg") as SVGSVGElement;
  svg.setAttribute("class", "wf-dg-svg");
  svg.setAttribute("viewBox", "0 0 100 50");
  const group = document.createElementNS(SVG_NS, "g");
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("class", "wf-dg-shape");
  path.setAttribute("d", "M0 0L10 10Z");
  const text = document.createElementNS(SVG_NS, "text");
  text.setAttribute("class", "wf-dg-text");
  text.textContent = "Order received";
  group.append(path, text);
  svg.append(group);
  document.body.append(svg);
  return svg;
}

afterEach(() => {
  document.head.innerHTML = "";
  document.body.innerHTML = "";
});

describe("standaloneSvg", () => {
  it("inlines the paint that only existed in CSS", () => {
    const markup = standaloneSvg(mounted(), 100, 50);
    // Serializing the node as-is would carry the class but none of its paint,
    // giving an unstyled skeleton in any other viewer.
    expect(markup).toContain("fill:rgb(1, 2, 3)");
    expect(markup).toContain("stroke:rgb(4, 5, 6)");
    expect(markup).toContain("fill:rgb(7, 8, 9)");
  });

  it("carries the namespace and explicit pixel dimensions", () => {
    const markup = standaloneSvg(mounted(), 320, 240);
    expect(markup).toContain(`xmlns="${SVG_NS}"`);
    expect(markup).toContain('width="320"');
    expect(markup).toContain('height="240"');
  });

  it("lays an opaque background from the theme behind everything", () => {
    const markup = standaloneSvg(mounted(), 100, 50);
    const background = markup.indexOf("<rect");
    const firstPath = markup.indexOf("<path");
    expect(background).toBeGreaterThan(-1);
    expect(background).toBeLessThan(firstPath);
    expect(markup).toContain('fill="#fdfcfa"');
  });

  it("leaves the live node untouched", () => {
    const svg = mounted();
    standaloneSvg(svg, 100, 50);
    expect(svg.querySelector("rect")).toBeNull();
    expect(svg.querySelector("path")!.getAttribute("style")).toBeNull();
    expect(svg.getAttribute("width")).toBeNull();
  });

  it("keeps the diagram's own content", () => {
    const markup = standaloneSvg(mounted(), 100, 50);
    expect(markup).toContain("Order received");
    expect(markup).toContain('d="M0 0L10 10Z"');
  });
});
