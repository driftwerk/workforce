// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { RichMarkdown } from "./RichMarkdown";
import { RichHostProvider } from "./host";
import { ALL_RICH_FEATURES, type RichFeatures } from "./fence";

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

afterEach(cleanup);

const fence = (lang: string, body: unknown) => `Here:\n\n\`\`\`${lang}\n${JSON.stringify(body)}\n\`\`\`\n`;
const CHART = fence("wf-chart", { kind: "bar", labels: ["a"], series: [{ data: [1] }] });
const IMAGE = fence("wf-image", { images: [{ artifact_id: "art_0123456789abcdef0123456789abcdef" }] });

const NONE: RichFeatures = {
  tables: false,
  charts: false,
  forms: false,
  maps: false,
  diagrams: false,
  images: false,
  excalidraw: false,
};

describe("RichMarkdown", () => {
  it("draws an enabled fence as its block", async () => {
    const { container } = render(<RichMarkdown text={CHART} />);
    await waitFor(() => expect(container.querySelector(".wf-chart")).not.toBeNull());
    expect(container.querySelector("pre")).toBeNull();
  });

  it("leaves a disabled fence as a code block", () => {
    const { container } = render(<RichMarkdown text={CHART} features={{ ...ALL_RICH_FEATURES, charts: false }} />);
    expect(container.querySelector(".wf-chart")).toBeNull();
    expect(container.querySelector("pre")?.textContent).toContain('"kind":"bar"');
  });

  it("renders wf-image for an agent with only images enabled", async () => {
    const { container } = render(<RichMarkdown text={IMAGE} features={{ ...NONE, images: true }} />);
    await waitFor(() => expect(container.querySelector(".wf-rimgs")).not.toBeNull());
  });

  it("closes a fence still open mid-stream instead of swallowing the text", () => {
    const { container } = render(<RichMarkdown text={"Before\n\n```wf-chart\n{"} streaming />);
    expect(container.querySelector(".wf-skel-chart")).not.toBeNull();
    expect(screen.getByText("Before")).toBeTruthy();
  });

  it("sanitizes agent markup", () => {
    const { container } = render(<RichMarkdown text={'<img src=x onerror="alert(1)"> **bold**'} />);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("strong")?.textContent).toBe("bold");
  });

  it("opens links through the host when it can", () => {
    const openUrl = vi.fn();
    render(
      <RichHostProvider host={{ openUrl }}>
        <RichMarkdown text="[docs](https://example.com/a)" />
      </RichHostProvider>,
    );
    const link = screen.getByRole("link", { name: "docs" });
    expect(fireEvent.click(link)).toBe(false);
    expect(openUrl).toHaveBeenCalledWith("https://example.com/a");
  });

  it("opens links in a new tab without a host opener", () => {
    render(<RichMarkdown text="[docs](https://example.com/a)" />);
    const link = screen.getByRole("link", { name: "docs" });
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    expect(fireEvent.click(link)).toBe(true);
  });
});

describe("RichMarkdown links", () => {
  const LINKS = "[the message](desk://room/r1/msg/m1) and [a trap](javascript:alert(1)) and [docs](https://example.com)";
  const hrefs = (container: HTMLElement) => [...container.querySelectorAll("a")].map((a) => a.getAttribute("href"));

  it("keeps a declared scheme and opens it through the host", () => {
    const openUrl = vi.fn();
    const { container } = render(
      <RichHostProvider host={{ openUrl, linkSchemes: ["desk"] }}>
        <RichMarkdown text={LINKS} />
      </RichHostProvider>,
    );
    expect(hrefs(container)).toContain("desk://room/r1/msg/m1");
    fireEvent.click(screen.getByText("the message"));
    expect(openUrl).toHaveBeenCalledWith("desk://room/r1/msg/m1");
  });

  it("reads a declared scheme however it is written", () => {
    const { container } = render(
      <RichHostProvider host={{ linkSchemes: [" Desk: "] }}>
        <RichMarkdown text={LINKS} />
      </RichHostProvider>,
    );
    expect(hrefs(container)).toContain("desk://room/r1/msg/m1");
  });

  it("strips a scheme no host declared", () => {
    const { container } = render(<RichMarkdown text={LINKS} />);
    expect(hrefs(container)).not.toContain("desk://room/r1/msg/m1");
    expect(hrefs(container)).toContain("https://example.com");
  });

  it("never keeps javascript:, declared or not", () => {
    for (const linkSchemes of [["desk"], ["desk", "javascript"]]) {
      const { container, unmount } = render(
        <RichHostProvider host={{ linkSchemes }}>
          <RichMarkdown text={LINKS} />
        </RichHostProvider>,
      );
      expect(hrefs(container).some((h) => h?.toLowerCase().startsWith("javascript"))).toBe(false);
      unmount();
    }
  });
});
