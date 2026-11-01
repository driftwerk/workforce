// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { RichMarkdown } from "./RichMarkdown";
import { RichHostProvider, type RichHost } from "./host";
import { dataImageOk, MAX_DATA_IMAGE, remoteImageLabel, remoteImageUrl } from "./images";

afterEach(cleanup);

const PNG = "data:image/png;base64,iVBORw0KGgo=";

const show = (text: string, host?: RichHost) =>
  render(
    host ? (
      <RichHostProvider host={host}>
        <RichMarkdown text={text} />
      </RichHostProvider>
    ) : (
      <RichMarkdown text={text} />
    ),
  );
// A source the policy drops leaves an `<img>` with no `src`: nothing to fetch.
const imgs = (c: HTMLElement) =>
  [...c.querySelectorAll("img")].map((i) => i.getAttribute("src")).filter((src): src is string => !!src);
const hrefs = (c: HTMLElement) => [...c.querySelectorAll("a")].map((a) => a.getAttribute("href"));

describe("remote images as links", () => {
  const LINK: RichHost = { remoteImages: "link" };

  it("shows a remote image as a link with an icon and its alt text, and fetches nothing", () => {
    const { container } = show("![A burndown chart](https://cdn.example.com/b.png?x=1)", LINK);
    expect(container.querySelector("img")).toBeNull();
    const link = screen.getByRole("link", { name: "A burndown chart" });
    expect(link.getAttribute("href")).toBe("https://cdn.example.com/b.png?x=1");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    expect(link.classList.contains("wf-remote-image")).toBe(true);
    expect(link.querySelector("svg")).not.toBeNull();
  });

  it("names the host when there is no alt text", () => {
    show("![](https://cdn.example.com/b.png)", LINK);
    expect(screen.getByRole("link", { name: "cdn.example.com" })).not.toBeNull();
  });

  it("opens through the host's link handler", () => {
    const openUrl = vi.fn();
    show("![chart](https://cdn.example.com/b.png)", { ...LINK, openUrl });
    expect(fireEvent.click(screen.getByRole("link", { name: "chart" }))).toBe(false);
    expect(openUrl).toHaveBeenCalledWith("https://cdn.example.com/b.png");
  });

  it("shows alt text as text, shortened when long", () => {
    const { container } = show(`![<b>bold</b> ${"x".repeat(200)}](https://e.example.com/a.png)`, LINK);
    expect(container.querySelector("b")).toBeNull();
    const label = container.querySelector(".wf-remote-image span")!.textContent!;
    expect(label.startsWith("<b>bold</b>")).toBe(true);
    expect(label.length).toBeLessThanOrEqual(80);
  });

  it("treats a protocol-relative address as remote", () => {
    const { container } = show("![t](//tracker.example.com/p.gif)", LINK);
    expect(container.querySelector("img")).toBeNull();
    expect(hrefs(container)).toEqual(["https://tracker.example.com/p.gif"]);
  });

  it("is one link, not a link in a link, when the image is linked", () => {
    const { container } = show("[![shot](https://cdn.example.com/s.png)](https://example.com/page)", LINK);
    expect(container.querySelectorAll("a")).toHaveLength(1);
    expect(hrefs(container)).toEqual(["https://example.com/page"]);
    expect(container.querySelector("a .wf-remote-image")?.textContent).toBe("shot");
  });

  it("still draws the host's own images", () => {
    const text = `![a](blob:null/1d2c) ![b](${PNG}) ![c](deskfile://localhost/hub/agent/art_1) ![d](http://deskfile.localhost/hub/agent/art_2) ![e](http://127.0.0.1:9/x.png)`;
    const { container } = show(text, LINK);
    expect(imgs(container)).toEqual([
      "blob:null/1d2c",
      PNG,
      "deskfile://localhost/hub/agent/art_1",
      "http://deskfile.localhost/hub/agent/art_2",
      "http://127.0.0.1:9/x.png",
    ]);
    expect(container.querySelectorAll("a")).toHaveLength(0);
  });

  it("draws an image from a scheme the host declared", () => {
    const { container } = show("![a](desk://pic/1) ![b](other://pic/2)", { ...LINK, linkSchemes: ["desk"] });
    expect(imgs(container)).toEqual(["desk://pic/1"]);
  });
});

describe("images drawn", () => {
  it("draws a remote image unless the host says otherwise", () => {
    for (const host of [undefined, {}, { remoteImages: "render" as const }]) {
      const { container, unmount } = show("![chart](https://cdn.example.com/b.png)", host);
      expect(imgs(container)).toEqual(["https://cdn.example.com/b.png"]);
      expect(container.querySelectorAll("a")).toHaveLength(0);
      unmount();
    }
  });

  it("draws the host's own sources whatever the policy", () => {
    const { container } = show(`![a](blob:null/1d2c) ![b](${PNG}) ![c](deskfile://localhost/h/a/art)`);
    expect(imgs(container)).toEqual(["blob:null/1d2c", PNG, "deskfile://localhost/h/a/art"]);
  });

  it("drops a source that runs or embeds content, declared or not", () => {
    const text = [
      "![a](javascript:alert(1))",
      "![b](data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=)",
      "![c](data:text/html;base64,PGI+aGk8L2I+)",
      "![d](data:image/png,notbase64)",
      "![e](file:///etc/passwd)",
    ].join(" ");
    for (const host of [undefined, { remoteImages: "link" as const, linkSchemes: ["javascript", "file", "data"] }]) {
      const { container, unmount } = show(text, host);
      expect(imgs(container)).toEqual([]);
      unmount();
    }
  });

  it("drops a data image over the size limit", () => {
    const big = `data:image/png;base64,${"A".repeat(MAX_DATA_IMAGE)}`;
    const { container } = show(`![big](${big})`);
    expect(imgs(container)).toEqual([]);
  });
});

describe("image sources", () => {
  it("accepts raster data images within the limit only", () => {
    expect(dataImageOk(PNG)).toBe(true);
    expect(dataImageOk("data:image/JPEG;base64,/9j/4AAQ")).toBe(true);
    expect(dataImageOk("data:image/svg+xml;base64,PHN2Zz4=")).toBe(false);
    expect(dataImageOk("data:image/png;base64,")).toBe(false);
    expect(dataImageOk(`data:image/png;base64,${"A".repeat(MAX_DATA_IMAGE)}`)).toBe(false);
  });

  it("finds the address a remote image is fetched from", () => {
    expect(remoteImageUrl("https://a.example.com/x.png")?.hostname).toBe("a.example.com");
    expect(remoteImageUrl("HTTP://a.example.com/x.png")?.hostname).toBe("a.example.com");
    expect(remoteImageUrl("//a.example.com/x.png")?.protocol).toBe("https:");
    expect(remoteImageUrl("/\\a.example.com/x.png")?.hostname).toBe("a.example.com");
    for (const none of [
      undefined,
      "",
      "relative/x.png",
      "blob:null/1",
      "data:image/png;base64,AAAA",
      "http://localhost:3000/x.png",
      "http://app.localhost/x.png",
      "http://127.0.0.1/x.png",
      "http://[::1]/x.png",
    ])
      expect(remoteImageUrl(none), String(none)).toBeNull();
  });

  it("labels with alt text, else the host", () => {
    const url = new URL("https://a.example.com/x.png");
    expect(remoteImageLabel("  two   words ", url)).toBe("two words");
    expect(remoteImageLabel("   ", url)).toBe("a.example.com");
    expect(remoteImageLabel(undefined, url)).toBe("a.example.com");
  });
});
