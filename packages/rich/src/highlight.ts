// The highlighter, reached only through a lazy `import()` so neither Shiki nor
// any grammar is in the package's initial bundle. Shiki runs on its JavaScript
// regex engine (no WASM to fetch) with its CSS-variables theme, and its tokens
// are turned into class names from a fixed list: the colours are the host's
// `--rich-code-*` tokens, and nothing the agent wrote can name a class or a
// style. Tokens are rendered as React text, never as HTML.

import { createCssVariablesTheme, createHighlighterCore, type HighlighterCore, type ThemedToken } from "@shikijs/core";
import { createJavaScriptRegexEngine } from "@shikijs/engine-javascript";

import { grammarLoader } from "./codeLang";
import { retryImport } from "./lazyRetry";

export interface CodeToken {
  content: string;
  /** Space-separated `wf-tok-*` classes, or undefined for plain text. */
  className?: string;
}

export type Tokenize = (code: string) => CodeToken[][];

const THEME = "wf-rich";
const PREFIX = "--wf-hl-";

const theme = createCssVariablesTheme({ name: THEME, variablePrefix: PREFIX, fontStyle: true });

// Every colour the CSS-variables theme can give a token. `foreground` is left
// out on purpose: such a token just inherits the block's text colour.
const KINDS = new Set([
  "comment",
  "keyword",
  "string",
  "string-expression",
  "constant",
  "function",
  "parameter",
  "punctuation",
  "link",
  "inserted",
  "deleted",
  "changed",
]);

const COLOR = new RegExp(`^var\\(${PREFIX}token-([a-z-]+)\\)$`);

// Shiki's FontStyle bit flags. Bold is dropped: a bolder weight can be wider in
// some monospace fonts, and the highlighted block must not reflow.
const ITALIC = 1;
const UNDERLINE = 4;

function toToken(token: ThemedToken): CodeToken {
  const classes: string[] = [];
  const kind = token.color ? COLOR.exec(token.color)?.[1] : undefined;
  if (kind && KINDS.has(kind)) classes.push(`wf-tok-${kind}`);
  const style = token.fontStyle ?? 0;
  if (style > 0 && style & ITALIC) classes.push("wf-tok-i");
  if (style > 0 && style & UNDERLINE) classes.push("wf-tok-u");
  return classes.length ? { content: token.content, className: classes.join(" ") } : { content: token.content };
}

let core: Promise<HighlighterCore> | null = null;
const grammars = new Map<string, Promise<void>>();

function highlighter(): Promise<HighlighterCore> {
  core ??= createHighlighterCore({
    themes: [theme],
    langs: [],
    engine: createJavaScriptRegexEngine({ forgiving: true }),
  }).catch((error: unknown) => {
    core = null;
    throw error;
  });
  return core;
}

/** A synchronous tokenizer for `grammar`, once it and the engine are loaded.
 *  Rejects for a grammar this package does not ship; a failed load is not
 *  cached, so the next block retries it. */
export async function loadTokenizer(grammar: string): Promise<Tokenize> {
  const load = grammarLoader(grammar);
  if (!load) throw new Error(`no grammar for ${grammar}`);
  const h = await highlighter();
  let ready = grammars.get(grammar);
  if (!ready) {
    ready = retryImport(load)
      .then((mod) => h.loadLanguage(...mod.default))
      .catch((error: unknown) => {
        grammars.delete(grammar);
        throw error;
      });
    grammars.set(grammar, ready);
  }
  await ready;
  return (code) =>
    h
      .codeToTokens(code, { lang: grammar, theme: THEME })
      .tokens.map((line) => line.filter((t) => t.content.length > 0).map(toToken));
}
