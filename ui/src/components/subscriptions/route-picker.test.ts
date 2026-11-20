import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { RouteModelsPicker } from "./route-picker";
import type { RouteModels, RouteOption } from "@/lib/api/llm-route";

const claude: RouteOption = {
  kind: "claude-code",
  title: "Claude",
  refused: null,
  models: ["fable", "opus", "claude-fable-5-1", "claude-haiku-4-5-20251001"],
  choices: [
    { id: "fable", name: "Fable 5.2", same_as: ["claude-fable-5-2"] },
    { id: "opus", name: "Opus 5.5" },
    { id: "claude-fable-5-1", name: "Fable 5.1" },
    { id: "claude-haiku-4-5-20251001", name: "Haiku 4.5" },
  ],
  models_from: "login",
  models_note: null,
  defaults: {
    conversation: "the agent's own tier",
    light: "the agent's light tier",
    heavy: "the agent's heavy tier",
  },
};

function render(option: RouteOption, route: RouteModels = {}): string {
  return renderToStaticMarkup(
    createElement(RouteModelsPicker, { name: "t", option, route, pending: false, onChange: () => {} }),
  );
}

/** The options of the picker for `work`, as [value, label]. */
function choices(html: string, work: string): [string, string][] {
  const select = html.match(new RegExp(`<select id="t-${work}"[^>]*>(.*?)</select>`))?.[1] ?? "";
  return [...select.matchAll(/<option value="([^"]*)"[^>]*>(.*?)<\/option>/g)].map((m) => [m[1] ?? "", m[2] ?? ""]);
}

describe("a route's model picker", () => {
  it("offers the default, the tiers, then the login's models by name in its order", () => {
    const shown = choices(render(claude), "light");
    expect(shown).toEqual([
      ["", "Default (the agent&#x27;s light tier)"],
      ["chat", "Same as conversation"],
      ["cheap", "Cheapest tier"],
      ["standard", "Everyday tier"],
      ["powerful", "Most capable tier"],
      ["fable", "Fable 5.2"],
      ["opus", "Opus 5.5"],
      ["claude-fable-5-1", "Fable 5.1"],
      ["claude-haiku-4-5-20251001", "Haiku 4.5"],
    ]);
    expect(choices(render(claude), "conversation").some(([v]) => v === "chat")).toBe(false);
  });

  it("keeps a chosen model the login no longer lists, shown as chosen", () => {
    const html = render(claude, { model_preferences: { conversation: "claude-opus-4-1" } });
    const shown = choices(html, "conversation");
    expect(shown.at(-1)).toEqual(["claude-opus-4-1", "claude-opus-4-1"]);
    expect(html).toMatch(/<option value="claude-opus-4-1" selected="">claude-opus-4-1<\/option>/);
    expect(choices(html, "light").some(([v]) => v === "claude-opus-4-1")).toBe(false);
  });

  it("reads a full id saved behind an alias as that model, once", () => {
    const html = render(claude, { model_preferences: { conversation: "claude-fable-5-2" } });
    const shown = choices(html, "conversation");
    expect(shown.filter(([, label]) => label === "Fable 5.2")).toEqual([["claude-fable-5-2", "Fable 5.2"]]);
    expect(shown.some(([v]) => v === "fable")).toBe(false);
    expect(html).toMatch(/<option value="claude-fable-5-2" selected="">Fable 5.2<\/option>/);
    expect(choices(html, "light")).toContainEqual(["fable", "Fable 5.2"]);
  });

  it("selects a listed model by its id without repeating it", () => {
    const html = render(claude, { heavy_model: "claude-fable-5-1" });
    const shown = choices(html, "heavy");
    expect(shown.filter(([v]) => v === "claude-fable-5-1")).toEqual([["claude-fable-5-1", "Fable 5.1"]]);
  });

  it("says why the hub's models stand in when the login's couldn't be read", () => {
    const html = render({
      ...claude,
      choices: [{ id: "opus", name: "opus" }],
      models: ["opus"],
      models_from: "hub",
      models_note: "Your Claude login's models couldn't be read just now, so the hub's own Claude models are offered.",
    });
    expect(html).toContain(
      '<p class="text-xs text-muted-foreground">Your Claude login&#x27;s models couldn&#x27;t be read just now, so the hub&#x27;s own Claude models are offered.</p>',
    );
    expect(render(claude)).not.toContain("couldn&#x27;t be read");
  });

  it("offers a hub's bare ids when it sends no names", () => {
    const old: RouteOption = { kind: "claude-code", title: "Claude", refused: null, models: ["opus", "sonnet"] };
    expect(choices(render(old), "heavy").slice(-2)).toEqual([
      ["opus", "opus"],
      ["sonnet", "sonnet"],
    ]);
  });
});
