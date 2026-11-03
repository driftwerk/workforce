// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { RichMarkdown } from "./RichMarkdown";
import { RichHostProvider, type RemoteImage, type RichHost } from "./host";
import { dataImageOk, localImageSource, MAX_DATA_IMAGE, remoteImageLabel, remoteImageUrl } from "./images";

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
    const text = `![a](blob:null/1d2c) ![b](${PNG}) ![c](deskfile://localhost/hub/agent/art_1) ![d](http://deskfile.localhost/hub/agent/art_2) ![e](https://DESKFILE.localhost/hub/agent/art_3) ![f](pics/own.png)`;
    const { container } = show(text, LINK);
    expect(imgs(container)).toEqual([
      "blob:null/1d2c",
      PNG,
      "deskfile://localhost/hub/agent/art_1",
      "http://deskfile.localhost/hub/agent/art_2",
      "https://DESKFILE.localhost/hub/agent/art_3",
      "pics/own.png",
    ]);
    expect(container.querySelectorAll("a")).toHaveLength(0);
  });

  it("links, never fetches, any other address on this computer", () => {
    // Loopback services and other `*.localhost` names are not the host's own:
    // an image there would be a GET to some other program.
    const sources = [
      "http://127.0.0.1:9/x.png",
      "http://localhost:4100/api/x",
      "http://evil.localhost/x",
      "http://deskfile.localhost.evil.example/x",
      "http://2130706433/x",
      "http://0x7f000001:8080/x",
      "http://0.0.0.0:4100/x",
    ];
    const { container } = show(sources.map((s, i) => `![p${i}](${s})`).join(" "), LINK);
    expect(imgs(container)).toEqual([]);
    expect(container.querySelectorAll("a.wf-remote-image")).toHaveLength(sources.length);
  });

  it("fails closed on a source it cannot read", () => {
    // Markdown percent-encodes the brackets, so these hosts no longer parse.
    const { container } = show("![a](http://[::1]:4100/a) ![b](http://[::ffff:127.0.0.1]/a)", LINK);
    expect(imgs(container)).toEqual([]);
  });

  it("never draws an image from a link scheme, declared or not", () => {
    const { container } = show("![a](desk://pic/1) ![b](other://pic/2)", { ...LINK, linkSchemes: ["desk"] });
    expect(imgs(container)).toEqual([]);
    const rendered = show("![a](desk://pic/1)", { linkSchemes: ["desk"] });
    expect(imgs(rendered.container)).toEqual([]);
  });
});

describe("remote images the host draws", () => {
  const drawn = vi.fn((image: RemoteImage) => (
    <button type="button" data-url={image.url}>
      Show {image.label} from {image.site}
    </button>
  ));
  const HOST: RichHost = { remoteImages: "link", remoteImage: drawn };
  afterEach(() => drawn.mockClear());

  it("hands the host the image's address, site and alt text, and fetches nothing", () => {
    const { container } = show("Here: ![A  burndown\nchart](https://cdn.example.com/b.png?x=1) done", HOST);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("a")).toBeNull();
    expect(drawn).toHaveBeenCalledWith({
      url: "https://cdn.example.com/b.png?x=1",
      site: "cdn.example.com",
      alt: "A burndown chart",
      label: "A burndown chart",
    });
    expect(screen.getByRole("button").textContent).toBe("Show A burndown chart from cdn.example.com");
  });

  it("reads a protocol-relative address as https", () => {
    show("![](//cdn.example.com/b.png)", HOST);
    expect(drawn).toHaveBeenCalledWith({ url: "https://cdn.example.com/b.png", site: "cdn.example.com", alt: "", label: "cdn.example.com" });
  });

  it("keeps an image inside a link as that link's text", () => {
    const { container } = show("[![chart](https://cdn.example.com/b.png)](https://example.com/report)", HOST);
    expect(drawn).not.toHaveBeenCalled();
    expect(container.querySelector("button")).toBeNull();
    expect(screen.getByRole("link", { name: "chart" }).getAttribute("href")).toBe("https://example.com/report");
  });

  it("isn't asked for the host's own images, or in render mode", () => {
    show(`![local](${PNG}) ![blob](blob:http://x/1)`, HOST);
    show("![chart](https://cdn.example.com/b.png)", { remoteImage: drawn });
    expect(drawn).not.toHaveBeenCalled();
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

  it("drops a data image dressed up past the raster check", () => {
    const text = [
      "![a](DATA:image/png;base64,iVBORw0KGgo=)",
      "![b](data:image/png;charset=utf-8;base64,iVBORw0KGgo=)",
      "![c](<data:image/png;base64, iVBORw0KGgo=>)",
      "![d](data:image/png;base64,iVBORw0KGgo=%0A)",
      "![e](data:image/svg+xml;charset=utf-8;base64,PHN2Zz4=)",
      "![f](data:image/png+svg;base64,PHN2Zz4=)",
      "![g](jav&#x09;ascript:alert(1))",
    ].join(" ");
    for (const host of [undefined, { remoteImages: "link" as const }]) {
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
      "http://deskfile.localhost/h/a/x",
    ])
      expect(remoteImageUrl(none), String(none)).toBeNull();
    for (const local of ["http://localhost:3000/x.png", "http://app.localhost/x.png", "http://127.0.0.1/x.png", "http://[::1]/x.png"])
      expect(remoteImageUrl(local)?.protocol, local).toBe("http:");
  });

  it("knows the sources a link-mode host may still draw", () => {
    for (const ok of ["blob:null/1", "deskfile://localhost/h/a/x", "http://deskfile.localhost/h/a/x", PNG, "pics/a.png", "/pics/a.png"])
      expect(localImageSource(ok), ok).toBe(true);
    for (const no of [
      "//evil.example/x",
      "\\\\evil.example/x",
      "http://127.0.0.1/x",
      "https://evil.example/x",
      "desk://pic/1",
      "data:image/svg+xml;base64,PHN2Zz4=",
      "javascript:alert(1)",
      "http://[bad/x",
    ])
      expect(localImageSource(no), no).toBe(false);
  });

  it("labels with alt text, else the host", () => {
    const url = new URL("https://a.example.com/x.png");
    expect(remoteImageLabel("  two   words ", url)).toBe("two words");
    expect(remoteImageLabel("   ", url)).toBe("a.example.com");
    expect(remoteImageLabel(undefined, url)).toBe("a.example.com");
  });
});
