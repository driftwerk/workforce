// Inspect a react-markdown `pre` hast node for a wf-* fenced block. Structural
// types only — no dependency on the hast packages.

export type RichKind = "table" | "chart" | "form" | "map" | "diagram" | "image";

/** Which wf-* blocks a host renders as components, named after the per-agent
 *  rich-output flags so a host can pass its agent's flags straight through. */
export interface RichFeatures {
  tables: boolean;
  charts: boolean;
  forms: boolean;
  maps: boolean;
  diagrams: boolean;
  images: boolean;
  /** Show the "download for Excalidraw" control on a rendered diagram. Not a
   *  fence gate — the model is never told about it. */
  excalidraw: boolean;
}

export const ALL_RICH_FEATURES: RichFeatures = {
  tables: true,
  charts: true,
  forms: true,
  maps: true,
  diagrams: true,
  images: true,
  excalidraw: true,
};

interface HastNode {
  type?: string;
  tagName?: string;
  properties?: { className?: unknown };
  children?: HastNode[];
  value?: string;
}

export interface FenceInfo {
  lang: string;
  text: string;
}

export interface CodeBlockInfo {
  /** The fence word, or `null` for a bare fence or an indented block. */
  lang: string | null;
  text: string;
}

/** The language (if any) + raw text of a `<pre>` whose sole child is a
 *  `<code>`, or `null` for anything else. */
export function codeBlockInfo(node: unknown): CodeBlockInfo | null {
  const pre = node as HastNode | undefined;
  const code = pre?.children?.length === 1 ? pre.children[0] : undefined;
  if (!code || code.tagName !== "code") return null;
  const classes = code.properties?.className;
  const lang =
    (Array.isArray(classes)
      ? classes
          .find((c): c is string => typeof c === "string" && c.startsWith("language-"))
          ?.slice("language-".length)
      : undefined) || null;
  const text = (code.children ?? [])
    .filter((c) => c.type === "text" && typeof c.value === "string")
    .map((c) => c.value)
    .join("");
  return { lang, text };
}

/** The fence language + raw payload of a `<pre>` whose sole child is a
 *  `<code class="language-…">`, or `null` for anything else. */
export function fenceInfo(node: unknown): FenceInfo | null {
  const block = codeBlockInfo(node);
  return block?.lang ? { lang: block.lang, text: block.text } : null;
}

/** Which rich renderer (if any) handles this fence, given the agent's flags. */
export function richKind(lang: string, features: RichFeatures): RichKind | null {
  if (lang === "wf-table" && features.tables) return "table";
  if (lang === "wf-chart" && features.charts) return "chart";
  if (lang === "wf-form" && features.forms) return "form";
  if (lang === "wf-map" && features.maps) return "map";
  if (lang === "wf-diagram" && features.diagrams) return "diagram";
  if (lang === "wf-image" && features.images) return "image";
  return null;
}

/** Whether any fence is rendered as a component at all. */
export const anyRichKind = (features: RichFeatures): boolean =>
  features.tables ||
  features.charts ||
  features.forms ||
  features.maps ||
  features.diagrams ||
  features.images;

/** While streaming, an as-yet-unclosed ``` fence would swallow the rest of the
 *  message as code. Balance it with a trailing fence so partial output renders. */
export function balanceFences(text: string): string {
  const fences = (text.match(/```/g) ?? []).length;
  return fences % 2 === 1 ? text + "\n```" : text;
}
