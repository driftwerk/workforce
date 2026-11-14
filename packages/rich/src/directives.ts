// Markdown directives (`:name[label]{a=b}`, `::name`, `:::name` … `:::`), for a
// host that draws them (`RichHost.directive`). Each directive becomes one
// `wf-directive` element carrying its kind, name, attributes and source, its
// label (a container's `[Title]`) marked apart from its content. One the host
// doesn't draw reads as the text it was written as: directive syntax catches
// prose too (`12:30`, `Note:this`), and that must read as written.

import type { ReactNode } from "react";

/** The element a directive is drawn through, and a container's label. */
export const DIRECTIVE_TAG = "wf-directive";
export const DIRECTIVE_LABEL_TAG = "wf-directive-label";

/** The properties a directive's element carries past the sanitizer. */
export const DIRECTIVE_PROPS = ["dataKind", "dataName", "dataAttrs", "dataSource", "dataOpen", "dataClose"] as const;

export type DirectiveKind = "text" | "leaf" | "container";

const KINDS: Record<string, DirectiveKind> = {
  textDirective: "text",
  leafDirective: "leaf",
  containerDirective: "container",
};

interface MdNode {
  type: string;
  name?: string;
  attributes?: Record<string, string | null | undefined> | null;
  children?: MdNode[];
  data?: Record<string, unknown> & { hName?: string; hProperties?: Record<string, unknown>; directiveLabel?: boolean };
  position?: { start: { offset?: number }; end: { offset?: number } };
}

/** `text` with markdown's backslash escapes of punctuation undone: what the
 *  reader of the source sees. */
export const unescapeMarkdown = (text: string): string => text.replace(/\\([!-/:-@[-`{-~])/g, "$1");

/** A container's opening and closing fence lines, as written. Unclosed (it ran
 *  to the end of the text), it has no closing line. */
const fenceLines = (source: string): { open: string; close: string } => {
  const lines = source.split("\n");
  const last = lines.length > 1 ? lines[lines.length - 1]!.trim() : "";
  return { open: lines[0]!.trim(), close: /^:{3,}$/.test(last) ? last : "" };
};

/** The remark plugin, after `remark-directive`: each directive made a
 *  `wf-directive` element, its source kept for the host that won't draw it. */
export function remarkWfDirectives() {
  return (tree: MdNode, file: { value?: unknown }) => {
    const text = typeof file.value === "string" ? file.value : String(file.value ?? "");
    const walk = (node: MdNode) => {
      const kind = KINDS[node.type];
      if (kind) {
        const start = node.position?.start.offset;
        const end = node.position?.end.offset;
        const source = start !== undefined && end !== undefined ? text.slice(start, end) : `:${node.name ?? ""}`;
        const attrs: Record<string, string> = {};
        for (const [k, v] of Object.entries(node.attributes ?? {})) attrs[k] = v ?? "";
        const props: Record<string, string> = {
          dataKind: kind,
          dataName: node.name ?? "",
          dataAttrs: JSON.stringify(attrs),
          dataSource: unescapeMarkdown(source),
        };
        if (kind === "container") {
          const { open, close } = fenceLines(source);
          props.dataOpen = unescapeMarkdown(open);
          props.dataClose = close;
          const first = node.children?.[0];
          if (first?.data?.directiveLabel) first.data.hName = DIRECTIVE_LABEL_TAG;
        }
        node.data = { ...node.data, hName: DIRECTIVE_TAG, hProperties: props };
      }
      node.children?.forEach(walk);
    };
    walk(tree);
  };
}

/** A directive as the host is asked to draw it. */
export interface Directive {
  kind: DirectiveKind;
  /** Its name: `mention` in `:mention[Ann]`. */
  name: string;
  /** Its `{…}` attributes, as written; one with no value is `""`. */
  attributes: Readonly<Record<string, string>>;
  /** Its `[…]` label drawn (a text or leaf directive's words, a container's
   *  title); null when it has none. */
  label: ReactNode | null;
  /** Its label's words as plain text; `""` when it has none. */
  labelText: string;
  /** A container's content, drawn; null for a text or leaf directive. */
  children: ReactNode | null;
}
