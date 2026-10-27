// Turn a live, CSS-styled <svg> into a standalone file.
//
// The diagram is painted entirely through CSS classes and theme variables (that
// is what makes it follow light/dark for free), so serializing the node as-is
// would produce an unstyled black-on-transparent skeleton. Every paint property
// is therefore resolved off the LIVE element and written inline on a clone,
// which is also why this takes a mounted node rather than a spec.

import { FALLBACK_BG } from "./palette";

/** The properties that carry the diagram's appearance. Deliberately a closed
 *  list: copying all of getComputedStyle would bloat the file enormously. */
const PAINT = [
  "fill",
  "fill-opacity",
  "stroke",
  "stroke-width",
  "stroke-opacity",
  "stroke-dasharray",
  "stroke-linecap",
  "stroke-linejoin",
  "font-family",
  "font-size",
  "font-weight",
  "text-anchor",
  "dominant-baseline",
] as const;

/** Devicepixel multiplier for the raster. 2x keeps text crisp when the PNG is
 *  dropped into a doc or a retina screen. */
const RASTER_SCALE = 2;

function inlinePaint(live: Element, clone: Element) {
  const computed = getComputedStyle(live);
  let style = "";
  for (const property of PAINT) {
    const value = computed.getPropertyValue(property);
    if (value) style += `${property}:${value};`;
  }
  if (style) clone.setAttribute("style", style);
  const liveChildren = live.children;
  const cloneChildren = clone.children;
  for (let i = 0; i < liveChildren.length && i < cloneChildren.length; i++) {
    inlinePaint(liveChildren[i], cloneChildren[i]);
  }
}

/** A self-contained SVG document string for the diagram, at its natural size and
 *  on an opaque background — a transparent PNG pasted into a dark doc would show
 *  dark text on dark. */
export function standaloneSvg(svg: SVGSVGElement, width: number, height: number): string {
  const clone = svg.cloneNode(true) as SVGSVGElement;
  inlinePaint(svg, clone);
  clone.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  clone.setAttribute("width", String(width));
  clone.setAttribute("height", String(height));

  const background = clone.ownerDocument.createElementNS(
    "http://www.w3.org/2000/svg",
    "rect",
  );
  background.setAttribute("x", "0");
  background.setAttribute("y", "0");
  background.setAttribute("width", "100%");
  background.setAttribute("height", "100%");
  background.setAttribute(
    "fill",
    getComputedStyle(svg).getPropertyValue("--rich-bg").trim() || FALLBACK_BG,
  );
  clone.insertBefore(background, clone.firstChild);

  return new XMLSerializer().serializeToString(clone);
}

/** Rasterize a standalone SVG string. The source is a `data:` URL, which is
 *  same-origin and carries no external reference, so the canvas is never
 *  tainted and `toBlob` succeeds. */
export function rasterize(markup: string, width: number, height: number): Promise<Blob> {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => {
      const canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.round(width * RASTER_SCALE));
      canvas.height = Math.max(1, Math.round(height * RASTER_SCALE));
      const context = canvas.getContext("2d");
      if (!context) return reject(new Error("no 2d context"));
      context.drawImage(image, 0, 0, canvas.width, canvas.height);
      canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("toBlob failed"))), "image/png");
    };
    image.onerror = () => reject(new Error("the diagram could not be rasterized"));
    image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(markup)}`;
  });
}
