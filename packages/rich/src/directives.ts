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

/** Whether a code ends a directive's name: nothing, a line end, a space, or
 *  punctuation other than `-` and `_` (as micromark-extension-directive reads
 *  a name). */
const endsName = (code: Code): boolean => {
  if (code === null || code < 0) return true;
  if (code === 45 || code === 95) return false;
  const c = String.fromCodePoint(code);
  return /\s/u.test(c) || /[\p{P}\p{S}]/u.test(c);
};

const isWordCode = (code: Code): boolean => code !== null && code >= 0 && /[\p{L}\p{N}]/u.test(String.fromCodePoint(code));

/** A look ahead at a directive's colons and name, failing unless the name is
 *  one of `names`: so a directive of another name fails on its first line,
 *  before it reads a thing (a container would otherwise read to its end, and
 *  only fail there). */
const nameProbe = (names: ReadonlySet<string>): Construct => ({
  partial: true,
  tokenize(effects, ok, nok) {
    let name = "";
    const colons: State = (code) => {
      if (code !== 58) return nok(code);
      effects.consume(code);
      return afterColons;
    };
    const afterColons: State = (code) => {
      if (code === 58) {
        effects.consume(code);
        return afterColons;
      }
      return read(code);
    };
    const read: State = (code) => {
      if (endsName(code)) {
        effects.exit("wfDirectiveProbe");
        return names.has(name.toLowerCase()) ? ok(code) : nok(code);
      }
      name += String.fromCodePoint(code!);
      effects.consume(code);
      return read;
    };
    return (code) => {
      effects.enter("wfDirectiveProbe");
      return colons(code);
    };
  },
});

/** `construct`, failing (so its text is read as anything else is) unless the
 *  name it reads is one of `names`, which is looked at before anything else.
 *  A text directive also fails right after a letter or digit (`mailto:x@y`,
 *  `see:mention[x]`): a directive starts a word. */
const onlyNamed = (construct: Construct, names: ReadonlySet<string>): Construct => {
  const probe = nameProbe(names);
  const previous = construct.previous as ((this: unknown, code: Code) => boolean) | undefined;
  return {
    ...construct,
    ...(previous
      ? {
          previous(this: unknown, code: Code) {
            return previous.call(this, code) && !isWordCode(code);
          },
        }
      : {}),
    tokenize(effects, ok, nok) {
      const real = construct.tokenize.call(this, effects, ok, nok);
      return (code) => (effects as unknown as { check(c: Construct, ok: State, nok: State): State }).check(probe, real, nok)(code);
    },
  };
};

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
        const props: Record<string, string> = { dataKind: kind, dataName: node.name ?? "", dataAttrs: JSON.stringify(attrs) };
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
  /** Its name, lowercase: `mention` in `:Mention[Ann]`. Always one of the
   *  host's `directiveNames`. */
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
