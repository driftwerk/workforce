// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { RichMarkdown } from "./RichMarkdown";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const fence = (lang: string, body: string) => "```" + lang + "\n" + body + "\n```\n";
// The grammar's first load compiles its patterns; give it room on a slow runner.
const LOAD = { timeout: 10_000 };

const lineTexts = (container: HTMLElement) =>
  [...container.querySelectorAll(".wf-code-line")].map((l) => l.textContent);

describe("CodeBlock", () => {
  it("highlights by the fence's language, with no layout change", async () => {
    const { container } = render(<RichMarkdown text={fence("ts", "const answer: number = 42; // why\nexport {}")} />);
    expect(container.querySelector(".wf-code-lang")?.textContent).toBe("TypeScript");
    // Plain at once, as one span per line…
    expect(lineTexts(container)).toEqual(["const answer: number = 42; // why", "export {}"]);
    expect(container.querySelector("[class^=wf-tok-]")).toBeNull();
    // …then the same lines, coloured.
    await waitFor(() => expect(container.querySelector(".wf-tok-keyword")?.textContent).toBe("const"), LOAD);
    expect(container.querySelector(".wf-tok-comment")?.textContent).toBe("// why");
    expect(lineTexts(container)).toEqual(["const answer: number = 42; // why", "export {}"]);
  });

  it("highlights each common language through its alias", async () => {
    const samples: [string, string][] = [
      ["rust", "fn main() {}"],
      ["py", "def f():\n    return 1"],
      ["bash", "echo hi"],
      ["yaml", "a: 1"],
    ];
    const { container } = render(<RichMarkdown text={samples.map(([l, b]) => fence(l, b)).join("\n")} />);
    await waitFor(() => {
      const blocks = [...container.querySelectorAll(".wf-code")];
      expect(blocks).toHaveLength(samples.length);
      for (const block of blocks) expect(block.querySelector("[class^=wf-tok-]")).not.toBeNull();
    }, LOAD);
  });

  it("an unknown language keeps its name and stays plain", async () => {
    const { container } = render(
      <RichMarkdown text={fence("brainfudge", "+++[>+<-]") + fence("ts", "let x = 1")} />,
    );
    const [unknown, known] = [...container.querySelectorAll(".wf-code")];
    expect(unknown!.querySelector(".wf-code-lang")?.textContent).toBe("brainfudge");
    // Once a known block in the same message has coloured, the unknown one
    // has had every chance to and still hasn't.
    await waitFor(() => expect(known!.querySelector(".wf-tok-keyword")).not.toBeNull(), LOAD);
    expect(unknown!.querySelector("[class^=wf-tok-]")).toBeNull();
    expect(unknown!.querySelector("code")?.textContent).toBe("+++[>+<-]");
  });

  it("a bare fence is plain text unless it parses as JSON", async () => {
    const { container } = render(<RichMarkdown text={fence("", "just words") + fence("", '{"ok": true}')} />);
    const [plain, json] = [...container.querySelectorAll(".wf-code")];
    expect(plain!.querySelector(".wf-code-lang")?.textContent).toBe("Text");
    expect(json!.querySelector(".wf-code-lang")?.textContent).toBe("JSON");
    await waitFor(() => expect(json!.querySelector("[class^=wf-tok-]")).not.toBeNull(), LOAD);
  });

  it("highlighting never lets agent markup through", async () => {
    const evil = '<img src=x onerror="alert(1)"><script>alert(2)</script>';
    const { container } = render(
      <RichMarkdown text={evil + "\n\n" + fence("html", evil) + fence("ts", "const s = '" + evil + "';")} />,
    );
    await waitFor(() => expect(container.querySelectorAll(".wf-code [class^=wf-tok-]").length).toBeGreaterThan(4), LOAD);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("[onerror]")).toBeNull();
    const codes = [...container.querySelectorAll(".wf-code code")].map((c) => c.textContent);
    expect(codes).toEqual([evil, "const s = '" + evil + "';"]);
    // Only the highlighter's own classes, and no inline styles at all.
    for (const el of container.querySelectorAll(".wf-code code *")) {
      expect(el.getAttribute("style")).toBeNull();
      for (const c of el.classList) expect(c).toMatch(/^wf-(code-line|tok-[a-z-]+)$/);
    }
  });

  it("a fence word cannot smuggle in a class", () => {
    const { container } = render(<RichMarkdown text={fence('ts" class="x', "1")} />);
    expect(container.querySelector(".x")).toBeNull();
  });

  it("copies the code, without the fence's newline, and says so", async () => {
    const writeText = vi.fn(() => Promise.resolve());
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    render(<RichMarkdown text={fence("sh", "npm ci\nnpm test")} />);
    const button = screen.getByRole("button", { name: "Copy" });
    fireEvent.click(button);
    expect(writeText).toHaveBeenCalledWith("npm ci\nnpm test");
    await waitFor(() => expect(button.textContent).toBe("Copied"));
  });

  it("says when the copy failed", async () => {
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText: () => Promise.reject(new Error("denied")) } });
    render(<RichMarkdown text={fence("sh", "x")} />);
    const button = screen.getByRole("button", { name: "Copy" });
    fireEvent.click(button);
    await waitFor(() => expect(button.textContent).toBe("Copy failed"));
  });

  it("colours a diff's added and removed lines", () => {
    const diff = ["diff --git a/x b/x", "--- a/x", "+++ b/x", "@@ -1,2 +1,2 @@", " same", "-old", "+new"].join("\n");
    const { container } = render(<RichMarkdown text={fence("diff", diff)} />);
    expect(container.querySelector(".wf-code-lang")?.textContent).toBe("Diff");
    const cls = (sel: string) => [...container.querySelectorAll(sel)].map((l) => l.textContent);
    expect(cls(".wf-code-add")).toEqual(["+new"]);
    expect(cls(".wf-code-del")).toEqual(["-old"]);
    expect(cls(".wf-code-hunk")).toEqual(["@@ -1,2 +1,2 @@"]);
    expect(cls(".wf-code-meta")).toEqual(["diff --git a/x b/x", "--- a/x", "+++ b/x"]);
  });

  it("keeps a streaming block coloured while its tail arrives", async () => {
    const head = "```ts\nconst a = 1;\nconst b = 2;\n";
    const { container, rerender } = render(<RichMarkdown text={head + "con"} streaming />);
    expect(lineTexts(container)).toEqual(["const a = 1;", "const b = 2;", "con"]);
    await waitFor(() => expect(container.querySelectorAll(".wf-tok-keyword").length).toBeGreaterThanOrEqual(2), LOAD);

    // The next token shows at once: the lines already coloured stay coloured
    // (no flash back to plain) and the new tail is plain text until the next
    // frame's pass.
    rerender(<RichMarkdown text={head + "const c = 3;\nlet d"} streaming />);
    expect(lineTexts(container)).toEqual(["const a = 1;", "const b = 2;", "const c = 3;", "let d"]);
    const lines = container.querySelectorAll(".wf-code-line");
    expect(lines[0]!.querySelector(".wf-tok-keyword")).not.toBeNull();
    expect(lines[1]!.querySelector(".wf-tok-keyword")).not.toBeNull();
    expect(lines[3]!.querySelector("[class^=wf-tok-]")).toBeNull();

    // A burst of tokens within one frame is folded into a single pass.
    for (const tail of ["let d ", "let d =", "let d = 4"]) {
      rerender(<RichMarkdown text={head + "const c = 3;\n" + tail} streaming />);
    }
    await waitFor(() => {
      const last = container.querySelectorAll(".wf-code-line")[3]!;
      expect(last.textContent).toBe("let d = 4");
      expect(last.querySelector(".wf-tok-keyword")?.textContent).toBe("let");
    });

    // The closing fence ends the stream; the block is whole and coloured.
    await act(async () => {
      rerender(<RichMarkdown text={head + "const c = 3;\nlet d = 4;\n```\n"} />);
    });
    await waitFor(() =>
      expect(container.querySelectorAll(".wf-code-line")[3]!.querySelector(".wf-tok-keyword")).not.toBeNull(),
    );
    expect(lineTexts(container)).toEqual(["const a = 1;", "const b = 2;", "const c = 3;", "let d = 4;"]);
  });

  it("leaves inline code alone", () => {
    const { container } = render(<RichMarkdown text="Run `npm ci` first." />);
    expect(container.querySelector(".wf-code")).toBeNull();
    expect(container.querySelector("p > code")?.textContent).toBe("npm ci");
  });
});
