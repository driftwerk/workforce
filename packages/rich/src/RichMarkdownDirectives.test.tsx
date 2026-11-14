// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import type { ReactNode } from "react";
import { RichMarkdown } from "./RichMarkdown";
import { RichHostProvider, type RichHost } from "./host";
import type { Directive } from "./directives";

afterEach(cleanup);

const draw = (host: RichHost, text: string) =>
  render(
    <RichHostProvider host={host}>
      <RichMarkdown text={text} />
    </RichHostProvider>,
  );

/** A host that draws a mention, a status and a panel, and nothing else. */
const directive = (d: Directive): ReactNode => {
  if (d.name === "mention") return <span data-testid="mention">@{d.labelText}</span>;
  if (d.name === "status") return <span data-testid="status" data-tone={d.attributes.tone}>{d.label}</span>;
  if (d.name === "panel")
    return (
      <div data-testid="panel" data-tone={d.attributes.tone}>
        {d.children}
      </div>
    );
  if (d.name === "details")
    return (
      <details data-testid="details">
        <summary>{d.label}</summary>
        {d.children}
      </details>
    );
  return undefined;
};

const NAMES = ["mention", "status", "panel", "details", "date", "aside", "video"];
const DRAWS: RichHost = { directive, directiveNames: NAMES };

/** What `text` draws as, as HTML, with no host. */
const plain = (text: string): string => {
  const { container, unmount } = render(<RichMarkdown text={text} />);
  const html = container.innerHTML;
  unmount();
  return html;
};

describe("directives", () => {
  it("are words when the host doesn't draw them: never parsed", () => {
    const { container } = render(<RichMarkdown text={"Note:this at 12:30, :mention[Ann] and :status[Done]{tone=success}"} />);
    expect(container.textContent).toBe("Note:this at 12:30, :mention[Ann] and :status[Done]{tone=success}");
  });

  it("are drawn by a host that draws them, with their label and attributes", () => {
    draw(DRAWS, "Ask :mention[Ann Lee], it's :status[**Done**]{tone=success}.");
    expect(screen.getByTestId("mention").textContent).toBe("@Ann Lee");
    const status = screen.getByTestId("status");
    expect(status.dataset.tone).toBe("success");
    expect(status.querySelector("strong")?.textContent).toBe("Done");
    // Inline, inside the paragraph.
    expect(status.closest("p")).not.toBeNull();
  });

  it("leave prose, links and autolinks after a colon exactly as without them", () => {
    for (const text of [
      "mailto:a@b.c",
      "user:name@example.com",
      "ref:jo@ex.com and ftp://a:b@c.d",
      "word:x[y](https://e.com)",
      "Due 10:30, Note:this, see https://example.com/a:b",
      ":::aside-not{x=1}\nInside **it**\n:::",
    ]) {
      const before = plain(text);
      const { container, unmount } = draw(DRAWS, text);
      expect(container.innerHTML, text).toBe(before);
      unmount();
    }
  });

  it("read as written, label and content drawn, where the host draws none of a name it declared", () => {
    const { container } = draw(DRAWS, "On :date[**2026**-11-30]{x=1 y} &amp; on");
    const p = container.querySelector("p")!;
    expect(p.textContent).toBe("On :date[2026-11-30]{x=1 y} & on");
    expect(p.querySelector("strong")?.textContent).toBe("2026");
  });

  it("keep escapes literal: an escaped brace or colon is that character", () => {
    const { container } = draw(DRAWS, "Keys \\{a\\} and \\:mention[x] stay words");
    expect(container.textContent).toBe("Keys {a} and :mention[x] stay words");
    expect(screen.queryByTestId("mention")).toBeNull();
  });

  it("give a container its content, and its label apart from it", () => {
    const text = "::::details[Rollout *notes*]\n:::panel{tone=info}\nGoes out **Monday**.\n:::\n\nAfter.\n::::";
    draw(DRAWS, text);
    const details = screen.getByTestId("details");
    expect(details.querySelector("summary")?.textContent).toBe("Rollout notes");
    expect(details.querySelector("summary em")?.textContent).toBe("notes");
    const panel = screen.getByTestId("panel");
    expect(panel.dataset.tone).toBe("info");
    expect(panel.textContent?.trim()).toBe("Goes out Monday.");
    // The label is not content too.
    expect(details.textContent).not.toMatch(/notes.*notes/);
    expect(details.textContent).toContain("After.");
  });

  it("hand the host a container's label as plain text", () => {
    const seen = vi.fn((_d: Directive) => undefined);
    draw({ directive: seen, directiveNames: NAMES }, ":::details[Big **one**]\nInside\n:::");
    const d = seen.mock.calls.find((c) => c[0].name === "details")![0];
    expect(d.kind).toBe("container");
    expect(d.labelText).toBe("Big one");
    expect(d.attributes).toEqual({});
  });

  it("show a container the host won't draw as its fence lines round its content", () => {
    const { container } = draw(DRAWS, "> ::::aside[Big *one*]{x=1}\n> Inside **it**\n> ::::");
    const ps = [...container.querySelectorAll("blockquote p")].map((p) => p.textContent);
    expect(ps).toEqual(["::::aside[Big one]{x=1}", "Inside it", "::::"]);
    expect(container.querySelector("blockquote p em")?.textContent).toBe("one");
  });

  it("show a leaf directive the host won't draw as its line", () => {
    const { container } = draw(DRAWS, "::video[clip]{src=x}");
    expect(container.querySelector("p")?.textContent).toBe("::video[clip]{src=x}");
  });

  it("are sanitized like the rest: a link in one keeps the scheme policy", () => {
    const { container } = draw(DRAWS, ":::panel{tone=info}\n[bad](javascript:alert(1)) <script>x</script>\n:::");
    const a = container.querySelector("a");
    expect(a?.getAttribute("href") ?? "").not.toContain("javascript");
    expect(container.querySelector("script")).toBeNull();
  });

  it("are read only for names the host declares, and only with its hook", () => {
    draw({ directive, directiveNames: ["status"] }, ":mention[Ann] :status[Done]");
    expect(screen.queryByTestId("mention")).toBeNull();
    expect(screen.getByTestId("status").textContent).toBe("Done");
    cleanup();
    const { container } = draw({ directiveNames: NAMES }, ":mention[Ann]");
    expect(container.textContent).toBe(":mention[Ann]");
    cleanup();
    draw({ directive, directiveNames: ["MENTION"] }, ":Mention[Ann]");
    expect(screen.getByTestId("mention").textContent).toBe("@Ann");
  });

  it("aren't read in fenced or inline code", () => {
    draw(DRAWS, "`:mention[Ann]`\n\n```\n:::panel{tone=info}\n:::\n```");
    expect(screen.queryByTestId("mention")).toBeNull();
    expect(screen.queryByTestId("panel")).toBeNull();
  });
});

describe("the image hook", () => {
  it("draws an image of a scheme the host declared", () => {
    const image = vi.fn((src: string, alt: string, _at: { inLink: boolean }) => (src.startsWith("desk-file:") ? <span data-testid="file">{alt}</span> : undefined));
    draw({ image, imageSchemes: ["desk-file"] }, "See ![shot.png](desk-file:abc-123) and ![web](https://example.com/x.png)");
    expect(screen.getByTestId("file").textContent).toBe("shot.png");
    expect(image.mock.calls.map((c) => c[0])).toEqual(["desk-file:abc-123", "https://example.com/x.png"]);
  });

  it("strips a scheme the host didn't declare before the hook sees it", () => {
    const image = vi.fn((_src: string, _alt: string, _at: { inLink: boolean }) => undefined);
    const { container } = draw({ image }, "![shot.png](desk-file:abc-123)");
    expect(image).not.toHaveBeenCalled();
    expect(container.querySelector("img")?.getAttribute("src") ?? "").toBe("");
  });

  it("draws an image of the host's scheme it didn't draw as its alt text, fetching nothing", () => {
    const { container } = draw({ image: () => undefined, imageSchemes: ["desk-file"] }, "![shot.png](desk-file:abc-123)");
    const img = container.querySelector("img");
    expect(img?.getAttribute("alt")).toBe("shot.png");
    expect(img?.hasAttribute("src")).toBe(false);
  });

  it("never keeps javascript: or data: as a host's image scheme", () => {
    const image = vi.fn((_src: string, _alt: string, _at: { inLink: boolean }) => undefined);
    draw({ image, imageSchemes: ["javascript", "file"] }, "![x](javascript:alert(1)) ![y](file:///etc/passwd)");
    expect(image).not.toHaveBeenCalled();
  });

  it("reads the host's image schemes as the policy does: any case, a trailing colon", () => {
    const { container } = draw({ image: () => undefined, imageSchemes: ["Desk-File:"] }, "![shot.png](desk-file:abc-123)");
    expect(container.querySelector("img")?.hasAttribute("src")).toBe(false);
    cleanup();
    const DATA = "data:image/png;base64,iVBORw0KGgo=";
    const drawn = draw({ image: () => undefined, imageSchemes: ["data"] }, `![dot](${DATA})`);
    expect(drawn.container.querySelector("img")?.getAttribute("src")).toBe(DATA);
  });

  it("tells the hook when the image is inside a link", () => {
    const image = vi.fn((_src: string, _alt: string, _at: { inLink: boolean }) => undefined);
    draw({ image, imageSchemes: ["desk-file"] }, "![a](desk-file:1) [![b](desk-file:2)](https://example.com)");
    expect(image.mock.calls.map((c) => [c[0], c[2].inLink])).toEqual([
      ["desk-file:1", false],
      ["desk-file:2", true],
    ]);
  });

  it("never makes a host's image scheme a link's", () => {
    const { container } = draw({ imageSchemes: ["desk-file"] }, "[x](desk-file:abc)");
    expect(container.querySelector("a")?.getAttribute("href") ?? "").toBe("");
  });
});
