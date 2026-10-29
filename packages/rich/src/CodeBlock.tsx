// A fenced code block: a header naming its language with a Copy button, and the
// code, which scrolls sideways inside the block rather than widening the page.
//
// The code is drawn plain at once and coloured when its grammar has loaded. The
// plain and the coloured block are the same DOM (one span per line), so the
// swap moves nothing. While a message streams, the block's text grows on every
// token: the lines already coloured stay coloured and only the new tail is
// plain until the next pass, which runs at most once per animation frame — and
// later than that when a pass is slow — so a long block never recolours in a
// loop or flickers back to plain.

import { memo, useEffect, useMemo, useRef, useState } from "react";

import { diffLine, resolveLang } from "./codeLang";
import type { CodeToken, Tokenize } from "./highlight";
import { retryImport } from "./lazyRetry";

/** Longer than this a block stays plain: tokenizing runs on the main thread. */
const HIGHLIGHT_MAX = 100_000;
/** A block whose grammar is already loaded is coloured in its first render up
 *  to this size, so a remounted message doesn't flash plain. */
const FIRST_RENDER_MAX = 20_000;

const loadHighlight = () => retryImport(() => import("./highlight"));

// Grammars loaded once stay loaded; remembering their tokenizers here is what
// lets a later block colour itself in its first render.
const tokenizers = new Map<string, Tokenize>();

function useTokenizer(grammar: string | null): Tokenize | null {
  const [loaded, setLoaded] = useState<{ grammar: string; tokenize: Tokenize } | null>(null);
  const cached = grammar ? tokenizers.get(grammar) : undefined;
  useEffect(() => {
    if (!grammar || tokenizers.has(grammar)) return;
    let live = true;
    loadHighlight()
      .then((mod) => mod.loadTokenizer(grammar))
      .then(
        (tokenize) => {
          tokenizers.set(grammar, tokenize);
          if (live) setLoaded({ grammar, tokenize });
        },
        // No grammar, no engine, no network: the block simply stays plain.
        () => {},
      );
    return () => {
      live = false;
    };
  }, [grammar]);
  if (cached) return cached;
  return loaded && loaded.grammar === grammar ? loaded.tokenize : null;
}

interface Highlighted {
  grammar: string;
  code: string;
  lines: CodeToken[][];
}

function tokenizeSafely(tokenize: Tokenize, code: string): CodeToken[][] | null {
  try {
    return tokenize(code);
  } catch {
    return null;
  }
}

function later(run: () => void, delay: number): () => void {
  const raf = typeof requestAnimationFrame === "function";
  let frame: number | undefined;
  const onFrame = () => {
    if (raf) frame = requestAnimationFrame(run);
    else frame = window.setTimeout(run, 16);
  };
  const timeout = delay > 0 ? window.setTimeout(onFrame, delay) : undefined;
  if (timeout === undefined) onFrame();
  return () => {
    if (timeout !== undefined) window.clearTimeout(timeout);
    if (frame === undefined) return;
    if (raf) cancelAnimationFrame(frame);
    else window.clearTimeout(frame);
  };
}

/** The block's tokens, kept as close to its current text as one pass per frame
 *  allows. */
function useHighlighted(grammar: string | null, code: string): Highlighted | null {
  const tokenize = useTokenizer(grammar);
  const [highlighted, setHighlighted] = useState<Highlighted | null>(() => {
    if (!grammar || !tokenize || code.length > FIRST_RENDER_MAX) return null;
    const lines = tokenizeSafely(tokenize, code);
    return lines && { grammar, code, lines };
  });

  const latest = useRef({ grammar, code, tokenize });
  latest.current = { grammar, code, tokenize };
  const cancel = useRef<(() => void) | null>(null);
  const cost = useRef(0);

  const stale =
    !!grammar &&
    !!tokenize &&
    code.length <= HIGHLIGHT_MAX &&
    (highlighted?.grammar !== grammar || highlighted.code !== code);

  // One pass at a time: the pass reads the latest text when it runs, so tokens
  // arriving faster than frames are folded into it instead of restarting it.
  useEffect(() => {
    if (!stale || cancel.current) return;
    // A slow pass (a long block) spaces the next one out, so streaming a big
    // file can't hold the main thread every frame.
    const delay = cost.current > 12 ? Math.min(cost.current * 3, 400) : 0;
    cancel.current = later(() => {
      cancel.current = null;
      const now = latest.current;
      if (!now.grammar || !now.tokenize || now.code.length > HIGHLIGHT_MAX) return;
      const start = performance.now();
      const lines = tokenizeSafely(now.tokenize, now.code);
      cost.current = performance.now() - start;
      if (lines) setHighlighted({ grammar: now.grammar, code: now.code, lines });
    }, delay);
  });

  useEffect(
    () => () => {
      cancel.current?.();
      cancel.current = null;
    },
    [],
  );

  return highlighted;
}

const Line = memo(function Line({ text, tokens, diff }: { text: string; tokens?: CodeToken[]; diff: boolean }) {
  const kind = diff ? diffLine(text) : null;
  return (
    <span className={kind ? `wf-code-line wf-code-${kind}` : "wf-code-line"}>
      {tokens
        ? tokens.map((t, i) =>
            t.className ? (
              <span key={i} className={t.className}>
                {t.content}
              </span>
            ) : (
              t.content
            ),
          )
        : text}
    </span>
  );
});

type CopyState = "idle" | "copied" | "failed";

function CopyButton({ text }: { text: string }) {
  const [state, setState] = useState<CopyState>("idle");
  useEffect(() => {
    if (state === "idle") return;
    const timer = window.setTimeout(() => setState("idle"), 1600);
    return () => window.clearTimeout(timer);
  }, [state]);
  return (
    <button
      type="button"
      className="wf-code-copy"
      aria-live="polite"
      onClick={() => {
        const clipboard = typeof navigator === "undefined" ? undefined : navigator.clipboard;
        if (!clipboard?.writeText) {
          setState("failed");
          return;
        }
        clipboard.writeText(text).then(
          () => setState("copied"),
          () => setState("failed"),
        );
      }}
    >
      {state === "copied" ? "Copied" : state === "failed" ? "Copy failed" : "Copy"}
    </button>
  );
}

/** A fenced code block. `lang` is the fence word as sanitized markdown left it
 *  (`null` for a bare fence); `text` is the block's text content. */
export function CodeBlock({ lang, text }: { lang: string | null; text: string }) {
  // The fence's closing newline is markdown's, not the code's.
  const code = useMemo(() => text.replace(/\r\n?/g, "\n").replace(/\n$/, ""), [text]);
  const resolved = useMemo(() => resolveLang(lang, code), [lang, code]);
  const highlighted = useHighlighted(resolved.grammar, code);
  const lines = code.split("\n");

  // Tokens still apply to the text they were made from; while the block grows,
  // that is every line but the last one they saw, which may since have grown.
  let covered = 0;
  if (highlighted && highlighted.grammar === resolved.grammar && code.startsWith(highlighted.code)) {
    covered = highlighted.code === code ? highlighted.lines.length : highlighted.lines.length - 1;
  }

  return (
    <div className="wf-code">
      <div className="wf-code-head">
        <span className="wf-code-lang">{resolved.label}</span>
        <CopyButton text={code} />
      </div>
      <pre className="wf-code-pre">
        <code>
          {lines.map((line, i) => (
            <Line
              key={i}
              text={line}
              tokens={i < covered ? highlighted?.lines[i] : undefined}
              diff={!!resolved.diff}
            />
          ))}
        </code>
      </pre>
    </div>
  );
}
