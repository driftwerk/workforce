// Markdown directives (`:name[label]{a=b}`, `::name`, `:::name` … `:::`), for a
// host that draws them (`RichHost.directive`). Only the names the host declares
// (`RichHost.directiveNames`) are read as directives: anything else that looks
// like one (`10:30`, `Note:this`, `mailto:a@b.c`) is never one, so it reads,
// links and autolinks exactly as it did. Each directive becomes one
// `wf-directive` element carrying its kind, name and attributes, its label (a
// container's `[Title]`) marked apart from its content.

import { directiveFromMarkdown } from "mdast-util-directive";
import { directive as directiveSyntax } from "micromark-extension-directive";
import type { ReactNode } from "react";

/** The element a directive is drawn through, and a container's label. */
export const DIRECTIVE_TAG = "wf-directive";
export const DIRECTIVE_LABEL_TAG = "wf-directive-label";

/** The properties a directive's element carries past the sanitizer. */
export const DIRECTIVE_PROPS = ["dataKind", "dataName", "dataAttrs", "dataColons", "dataClosed"] as const;

export type DirectiveKind = "text" | "leaf" | "container";

const KINDS: Record<string, DirectiveKind> = {
  textDirective: "text",
  leafDirective: "leaf",
  containerDirective: "container",
};

const NAME_TOKENS = new Set(["directiveTextName", "directiveLeafName", "directiveContainerName"]);

/** Directive names as a host's list declares them: trimmed, lowercase. */
export const directiveNameList = (names: string): string[] =>
  names
    .split(",")
    .map((n) => n.trim().toLowerCase())
    .filter(Boolean);

// micromark's types aren't a dependency here: the shapes this touches.
type Code = number | null;
type State = (code: Code) => State | undefined;
interface Effects {
  enter(type: string, fields?: unknown): unknown;
  exit(type: string): unknown;
  consume(code: Code): void;
  [k: string]: unknown;
}
interface Construct {
  tokenize: (this: unknown, effects: Effects, ok: State, nok: State) => State;
  [k: string]: unknown;
}

/** `construct`, failing (so its text is read as anything else is) unless the
 *  name it read is one of `names`. */
const onlyNamed = (construct: Construct, names: ReadonlySet<string>): Construct => ({
  ...construct,
  tokenize(effects, ok, nok) {
    let name = "";
    let inName = false;
    const watched: Effects = {
      ...effects,
      enter(type, fields) {
        if (NAME_TOKENS.has(type)) inName = true;
        return effects.enter(type, fields);
      },
      exit(type) {
        if (NAME_TOKENS.has(type)) inName = false;
        return effects.exit(type);
      },
      consume(code) {
        if (inName && code !== null && code >= 0) name += String.fromCharCode(code);
        effects.consume(code);
      },
    };
    const known: State = (code) => (names.has(name.toLowerCase()) ? ok(code) : nok(code));
    return construct.tokenize.call(this, watched, known, nok);
  },
});

/** The remark plugin that reads directives named `names` and nothing else. */
export function remarkNamedDirectives(this: { data(): object }, names: readonly string[]) {
  const allowed = new Set(names.map((n) => n.toLowerCase()));
  const syntax = directiveSyntax() as unknown as { text: Record<number, Construct>; flow: Record<number, Construct[]> };
  const only = {
    text: Object.fromEntries(Object.entries(syntax.text).map(([k, c]) => [k, onlyNamed(c, allowed)])),
    flow: Object.fromEntries(Object.entries(syntax.flow).map(([k, cs]) => [k, cs.map((c) => onlyNamed(c, allowed))])),
  };
  const data = this.data() as Record<string, unknown[] | undefined>;
  (data.micromarkExtensions ??= []).push(only);
  (data.fromMarkdownExtensions ??= []).push(directiveFromMarkdown());
}

interface MdNode {
  type: string;
  name?: string;
  attributes?: Record<string, string | null | undefined> | null;
  children?: MdNode[];
  data?: Record<string, unknown> & { hName?: string; hProperties?: Record<string, unknown>; directiveLabel?: boolean };
  position?: { start: { offset?: number }; end: { offset?: number } };
}

/** A line as written, without the quote markers (`> `) a container in a
 *  quote has in front of it. */
const unquoted = (line: string): string => line.replace(/^\s*(?:>\s?)*\s*/, "");

/** The remark plugin, after {@link remarkNamedDirectives}: each directive made
 *  a `wf-directive` element. A container keeps how many colons fence it and
 *  whether it was closed, so one the host won't draw reads as written. */
export function remarkWfDirectives() {
  return (tree: MdNode, file: { value?: unknown }) => {
    const text = typeof file.value === "string" ? file.value : String(file.value ?? "");
    const walk = (node: MdNode) => {
      const kind = KINDS[node.type];
      if (kind) {
        const attrs: Record<string, string> = {};
        for (const [k, v] of Object.entries(node.attributes ?? {})) attrs[k] = v ?? "";
        const props: Record<string, string> = { dataKind: kind, dataName: (node.name ?? "").toLowerCase(), dataAttrs: JSON.stringify(attrs) };
        if (kind === "container") {
          const start = node.position?.start.offset;
          const end = node.position?.end.offset;
          const lines = start !== undefined && end !== undefined ? text.slice(start, end).split("\n") : [":::"];
          props.dataColons = String(/^:+/.exec(unquoted(lines[0]!))?.[0].length ?? 3);
          props.dataClosed = lines.length > 1 && /^:{3,}\s*$/.test(unquoted(lines[lines.length - 1]!)) ? "true" : "";
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

/** A directive's attributes as written: `{tone=info x}`; none, nothing. */
export const attributesText = (attrs: Readonly<Record<string, string>>): string => {
  const parts = Object.entries(attrs).map(([k, v]) => (v === "" ? k : /^[^\s"'=<>`{}]+$/.test(v) ? `${k}=${v}` : v.includes('"') ? `${k}='${v}'` : `${k}="${v}"`));
  return parts.length ? `{${parts.join(" ")}}` : "";
};

/** A directive as the host is asked to draw it. */
export interface Directive {
  kind: DirectiveKind;
  /** Its name, lowercase as written: `mention` in `:mention[Ann]`. Always one
   *  of the host's `directiveNames`. */
  name: string;
  /** Its `{…}` attributes, as written; one with no value is `""`. They are the
   *  author's words: draw only values the host knows. */
  attributes: Readonly<Record<string, string>>;
  /** Its `[…]` label drawn (a text or leaf directive's words, a container's
   *  title); null when it has none. */
  label: ReactNode | null;
  /** Its label's words as plain text; `""` when it has none. */
  labelText: string;
  /** A container's content, drawn; null for a text or leaf directive. */
  children: ReactNode | null;
}
