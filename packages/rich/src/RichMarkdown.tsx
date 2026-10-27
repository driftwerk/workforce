// A message's markdown with its wf-* fences rendered as rich blocks. Agent
// output is untrusted, so the markdown is sanitized; wide tables are wrapped to
// scroll instead of overflowing, and links open through the host when it can,
// otherwise in a new tab.

import { useMemo, useRef, type ComponentProps } from "react";
import Markdown, { type Components, type ExtraProps } from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";

import {
  ALL_RICH_FEATURES,
  anyRichKind,
  balanceFences,
  fenceInfo,
  richKind,
  type RichFeatures,
} from "./fence";
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
  const components = useMemo<Components>(() => {
    if (!anyRichKind(features)) return MD_COMPONENTS;
    return {
      ...MD_COMPONENTS,
      pre: ({ node, ...props }) => {
        const fence = fenceInfo(node);
        const kind = fence && richKind(fence.lang, features);
        if (!kind) return <pre {...props} />;
        return (
          <RichBlock
            kind={kind}
            payload={fence.text}
            streaming={streamingRef.current}
            fallback={<pre {...props} />}
            excalidraw={features.excalidraw}
          />
        );
      },
    };
  }, [features]);
  return (
    <div className="wf-md">
      <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeSanitize]} components={components}>
        {balanceFences(text)}
      </Markdown>
    </div>
  );
}
