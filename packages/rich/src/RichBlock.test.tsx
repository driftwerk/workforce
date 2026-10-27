// @vitest-environment jsdom
//
// Routing cover for the one mistake this dispatcher invites: the chart arm is an
// unguarded `else`, so any spec shape not discriminated before it silently
// renders as a chart.

import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import { RichBlock } from "./RichBlock";

// jsdom implements neither API; the chart renderer needs both to mount at all.
beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
  });
  Object.defineProperty(window, "ResizeObserver", {
    writable: true,
    value: class {
      observe() {}
      disconnect() {}
    },
  });
});

const DIAGRAM = JSON.stringify({
  kind: "flow",
  title: "Order pipeline",
  nodes: [
    { id: "in", label: "Order received" },
    { id: "chk", label: "Stock available?", shape: "diamond" },
  ],
  edges: [{ from: "in", to: "chk" }],
});

const CHART = JSON.stringify({ kind: "bar", labels: ["a"], series: [{ data: [1] }] });

const IMAGE = JSON.stringify({
  images: [{ artifact_id: "art_0123456789abcdef0123456789abcdef", caption: "your selfie" }],
});

const show = (
  kind: "diagram" | "chart" | "image",
  payload: string,
  streaming = false,
  excalidraw = false,
) =>
  render(
    <RichBlock
      kind={kind}
      payload={payload}
      streaming={streaming}
      fallback={<pre>raw</pre>}
      excalidraw={excalidraw}
    />,
  );

const buttonLabels = (root: HTMLElement) =>
  [...root.querySelectorAll(".wf-dg-actions button")].map((b) => b.textContent);

afterEach(cleanup);

describe("RichBlock", () => {
  it("renders a diagram spec as a diagram, never as a chart", async () => {
    const { container } = show("diagram", DIAGRAM);
    await waitFor(() => expect(container.querySelector(".wf-dg")).not.toBeNull());
    expect(container.querySelector(".wf-chart")).toBeNull();
    expect(screen.getByRole("img").getAttribute("aria-label")).toContain("Flow diagram");
  });

  /// `{"images":[…]}` has no `mode`, `regions`, `nodes` or `participants`, so it
  /// falls through every discriminator to the unguarded chart arm unless it is
  /// matched first — the exact failure this file exists to catch.
  it("renders an image spec as images, never as a chart", async () => {
    const { container } = show("image", IMAGE);
    await waitFor(() => expect(container.querySelector(".wf-rimgs")).not.toBeNull());
    expect(container.querySelector(".wf-chart")).toBeNull();
  });

  it("falls back to the code block when an image spec names no usable id", () => {
    const { container } = show("image", JSON.stringify({ images: [{ artifact_id: "nope" }] }));
    expect(container.querySelector(".wf-rimgs")).toBeNull();
    expect(container.textContent).toContain("raw");
  });

  it("still routes a chart spec to the chart renderer", async () => {
    const { container } = show("chart", CHART);
    await waitFor(() => expect(container.querySelector(".wf-chart")).not.toBeNull());
    expect(container.querySelector(".wf-dg")).toBeNull();
  });

  it("holds a diagram at the skeleton until the message is final", () => {
    const { container } = show("diagram", DIAGRAM, true);
    expect(container.querySelector(".wf-skel-diagram")).not.toBeNull();
    expect(container.querySelector(".wf-dg")).toBeNull();
  });

  it("always offers the image download, and the Excalidraw one only when enabled", async () => {
    const off = show("diagram", DIAGRAM);
    await waitFor(() => expect(off.container.querySelector(".wf-dg")).not.toBeNull());
    expect(buttonLabels(off.container)).toEqual(["Download image"]);
    cleanup();

    const on = show("diagram", DIAGRAM, false, true);
    await waitFor(() => expect(on.container.querySelector(".wf-dg")).not.toBeNull());
    expect(buttonLabels(on.container)).toEqual(["Download image", "Download for Excalidraw"]);
  });

  it("degrades an invalid final diagram to the code block", () => {
    const { container } = show("diagram", '{"kind":"flow","nodes":[]}');
    expect(container.querySelector(".wf-dg")).toBeNull();
    expect(container.textContent).toContain("raw");
  });
});
