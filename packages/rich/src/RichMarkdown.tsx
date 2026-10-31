// A message's markdown with its wf-* fences rendered as rich blocks and its
// other fenced code highlighted. Agent output is untrusted, so the markdown is
// sanitized, and code is highlighted only after that, from the sanitized text,
// into React text; wide tables are wrapped to scroll instead of overflowing,
// and links open through the host when it can, otherwise in a new tab. A link
// keeps a scheme beyond the web's only when the host declares it.

import { useMemo, useRef, type ComponentProps } from "react";
import Markdown, { defaultUrlTransform, type Components, type ExtraProps, type Options } from "react-markdown";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import remarkGfm from "remark-gfm";

import { CodeBlock } from "./CodeBlock";
import { ALL_RICH_FEATURES, balanceFences, codeBlockInfo, richKind, type RichFeatures } from "./fence";
import { useHost } from "./host";
import { RichBlock } from "./RichBlock";

// `node` is react-markdown's AST handle — strip it so it isn't forwarded onto
// the DOM element.
function Link({ node, ...props }: ComponentProps<"a"> & ExtraProps) {
  void node;
  const { latest } = useHost();
  return (
    <a
      {...props}
      target="_blank"
      rel="noopener noreferrer"
      onClick={(e) => {
        const host = latest.current;
        if (!host.openUrl || !props.href) return;
        e.preventDefault();
        host.openUrl(props.href);
      }}
    />
  );
}

const MD_COMPONENTS: Components = {
  table: ({ node, ...props }) => {
    void node;
    return (
      <div className="wf-tablewrap">
        <table {...props} />
      </div>
    );
  },
  a: Link,
};

/** Schemes that run or embed content: never kept, whatever a host declares. */
const NEVER = new Set(["javascript", "vbscript", "data", "file"]);

const schemeOf = (url: string): string | null => {
  const colon = url.indexOf(":");
  if (colon < 1 || /[/?#]/.test(url.slice(0, colon))) return null;
  return url.slice(0, colon).toLowerCase();
};

/** The sanitizer and URL transform that keep `schemes` (comma-joined) on links,
 *  besides the defaults. */
function linkPolicy(schemes: string): {
  rehypePlugins: NonNullable<Options["rehypePlugins"]>;
  urlTransform: (url: string) => string;
} {
  const kept = schemes
    ? schemes
        .split(",")
        .map((s) => s.trim().toLowerCase().replace(/:$/, ""))
        .filter((s) => /^[a-z][a-z0-9+.-]*$/.test(s) && !NEVER.has(s))
    : [];
  if (kept.length === 0) return { rehypePlugins: [rehypeSanitize], urlTransform: defaultUrlTransform };
  const schema = {
    ...defaultSchema,
    protocols: { ...defaultSchema.protocols, href: [...(defaultSchema.protocols?.href ?? []), ...kept] },
  };
  return {
    rehypePlugins: [[rehypeSanitize, schema]],
    urlTransform: (url) => {
      const scheme = schemeOf(url);
      return scheme && kept.includes(scheme) ? url : defaultUrlTransform(url);
    },
  };
}

/** Render `text` as markdown, drawing each enabled wf-* fence as its block.
 *  `features` should be a stable object: a new one rebuilds the renderers and
 *  remounts every block. */
export function RichMarkdown({
  text,
  streaming = false,
  features = ALL_RICH_FEATURES,
}: {
  text: string;
  /** The message is still arriving: blocks show skeletons until they validate. */
  streaming?: boolean;
  features?: RichFeatures;
}) {
  // Read `streaming` through a ref so the components map (and thus the `pre`
  // renderer identity) stays stable when a turn ends — otherwise a chart that
  // rendered mid-stream would remount and replay its animation. Re-render on the
  // final token still re-invokes the renderer, so an invalid block still swaps
  // from skeleton to its code-block fallback.
  const streamingRef = useRef(streaming);
  streamingRef.current = streaming;
  const { schemes } = useHost();
  const links = useMemo(() => linkPolicy(schemes), [schemes]);
  const components = useMemo<Components>(
    () => ({
      ...MD_COMPONENTS,
      pre: ({ node, ...props }) => {
        const block = codeBlockInfo(node);
        if (!block) return <pre {...props} />;
        const code = <CodeBlock lang={block.lang} text={block.text} />;
        const kind = block.lang && richKind(block.lang, features);
        if (!kind) return code;
        return (
          <RichBlock
            kind={kind}
            payload={block.text}
            streaming={streamingRef.current}
            fallback={code}
            excalidraw={features.excalidraw}
          />
        );
      },
    }),
    [features],
  );
  return (
    <div className="wf-md">
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={links.rehypePlugins}
        urlTransform={links.urlTransform}
        components={components}
      >
        {balanceFences(text)}
      </Markdown>
    </div>
  );
}
