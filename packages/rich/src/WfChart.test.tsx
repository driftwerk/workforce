// @vitest-environment jsdom

import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { RichBlock } from "./RichBlock";

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

afterEach(cleanup);

const CHART = JSON.stringify({
  kind: "bar",
  title: "Signups by plan",
  labels: ["Free", "Pro"],
  series: [
    { name: "March", data: [420, 180] },
    { name: "April", data: [510, 220] },
  ],
});

const shown = async () => {
  const view = render(<RichBlock kind="chart" payload={CHART} streaming={false} fallback={<pre>raw</pre>} />);
  // The first chart waits for the lazy chunk, which can take a while cold.
  await waitFor(() => expect(view.container.querySelector(".wf-chart")).not.toBeNull(), { timeout: 5000 });
  return view;
};

const labels = (root: HTMLElement) =>
  [...root.querySelectorAll(".wf-chart-actions button")].map((b) => b.textContent);

describe("WfChart", () => {
  it("offers the data as a table, and the chart as an image to copy or save", async () => {
    const { container } = await shown();
    expect(labels(container)).toEqual(["Show data", "Copy image", "Save image"]);
    expect(container.querySelector(".wf-chart-plot")?.getAttribute("aria-label")).toBe("bar chart: Signups by plan");
  });

  it("shows the numbers behind the chart in place of the plot, and back", async () => {
    const { container } = await shown();
    fireEvent.click(screen.getByRole("button", { name: "Show data" }));
    const plot = container.querySelector<HTMLElement>(".wf-chart-plot")!;
    expect(plot.hidden).toBe(true);
    await waitFor(() => expect(container.querySelector("table")).not.toBeNull(), { timeout: 5000 });
    const headers = [...container.querySelectorAll("th")].map((th) => th.textContent ?? "");
    expect(headers.some((h) => h.includes("March")) && headers.some((h) => h.includes("April"))).toBe(true);
    expect(container.textContent).toContain("420");
    expect(screen.getByRole("button", { name: "Copy image" })).toHaveProperty("disabled", true);
    fireEvent.click(screen.getByRole("button", { name: "Show chart" }));
    expect(plot.hidden).toBe(false);
    expect(container.querySelector("table")).toBeNull();
  });

  it("says so when the image can't be copied", async () => {
    await shown();
    fireEvent.click(screen.getByRole("button", { name: "Copy image" }));
    expect((await screen.findByRole("alert")).textContent).toBe("The image couldn’t be copied.");
  });
});
