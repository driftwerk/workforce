import { describe, expect, it } from "vitest";

import { diffLine, GRAMMARS, grammarLoader, resolveLang } from "./codeLang";

describe("resolveLang", () => {
  it("names the language from the fence word, its aliases and any case", () => {
    expect(resolveLang("ts", "")).toEqual({ grammar: "typescript", label: "TypeScript" });
    expect(resolveLang("TypeScript", "")).toEqual({ grammar: "typescript", label: "TypeScript" });
    expect(resolveLang("rs", "").grammar).toBe("rust");
    expect(resolveLang("py", "").grammar).toBe("python");
    expect(resolveLang("bash", "").grammar).toBe("shellscript");
    expect(resolveLang("sh", "").grammar).toBe("shellscript");
    expect(resolveLang("yml", "").grammar).toBe("yaml");
    expect(resolveLang("golang", "").grammar).toBe("go");
    expect(resolveLang("tsx", "").grammar).toBe("tsx");
    expect(resolveLang("sql", "").grammar).toBe("sql");
    expect(resolveLang("toml", "").grammar).toBe("toml");
    expect(resolveLang("json", "").grammar).toBe("json");
  });

  it("draws diff from its lines, with no grammar", () => {
    expect(resolveLang("diff", "")).toEqual({ grammar: null, label: "Diff", diff: true });
    expect(resolveLang("patch", "").diff).toBe(true);
  });

  it("keeps an unknown fence word as the label and draws it plain", () => {
    expect(resolveLang("brainfudge", "+++")).toEqual({ grammar: null, label: "brainfudge" });
    expect(resolveLang("x".repeat(40), "").label).toHaveLength(24);
  });

  it("never resolves a prototype member", () => {
    for (const word of ["constructor", "__proto__", "toString", "hasOwnProperty"]) {
      expect(resolveLang(word, "").grammar).toBeNull();
      expect(grammarLoader(word)).toBeUndefined();
    }
  });

  it("guesses only JSON for a bare fence", () => {
    expect(resolveLang(null, '{"a": [1, 2]}')).toEqual({ grammar: "json", label: "JSON" });
    expect(resolveLang(null, "[1, 2]").grammar).toBe("json");
    expect(resolveLang(null, '{"a": ')).toEqual({ grammar: null, label: "Text" });
    expect(resolveLang(null, "const a = 1;")).toEqual({ grammar: null, label: "Text" });
    expect(resolveLang(null, "42").grammar).toBeNull();
  });
});

describe("grammars", () => {
  // Each loader is a literal import so the host's bundler splits it; this holds
  // every one to a module that really registers the grammar under that name.
  it("every loader registers the grammar it is listed under", async () => {
    for (const name of GRAMMARS) {
      const mod = await grammarLoader(name)!();
      expect(
        mod.default.map((g) => g.name),
        name,
      ).toContain(name);
    }
  }, 20_000);
});

describe("diffLine", () => {
  it("colours added, removed, hunk and header lines", () => {
    expect(diffLine("+new")).toBe("add");
    expect(diffLine("-old")).toBe("del");
    expect(diffLine("@@ -1,2 +1,2 @@")).toBe("hunk");
    expect(diffLine("+++ b/src/a.ts")).toBe("meta");
    expect(diffLine("--- a/src/a.ts")).toBe("meta");
    expect(diffLine("diff --git a/x b/x")).toBe("meta");
    expect(diffLine(" context")).toBeNull();
    expect(diffLine("")).toBeNull();
  });
});
