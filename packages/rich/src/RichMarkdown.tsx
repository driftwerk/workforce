// A message's markdown with its wf-* fences rendered as rich blocks and its
// other fenced code highlighted. Agent output is untrusted, so the markdown is
// sanitized, and code is highlighted only after that, from the sanitized text,
// into React text; wide tables are wrapped to scroll instead of overflowing,
// and links open through the host when it can, otherwise in a new tab. A link
// keeps a scheme beyond the web's only when the host declares it. An image is
// drawn from the host's own sources (blob:, deskfile:, a small raster data: URL)
// and from the web unless the host says remote images are links
// (`remoteImages: "link"`): then one is a link to it, or what the host draws
// for it (`remoteImage`), anything else is not drawn, and the page never
// fetches either. A host may draw its own links, images (of schemes it
// declares) and directives; directives are only read for a host that draws
// them, and one it doesn't draw reads as it was written.

import { Children, createContext, isValidElement, useContext, useMemo, useRef, type ComponentProps, type ReactNode } from "react";
import Markdown, { defaultUrlTransform, type Components, type ExtraProps, type Options } from "react-markdown";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import remarkGfm from "remark-gfm";

import { CodeBlock } from "./CodeBlock";
import { attributesText, directiveNameList, DIRECTIVE_LABEL_TAG, DIRECTIVE_PROPS, DIRECTIVE_TAG, remarkNamedDirectives, remarkWfDirectives, type DirectiveKind } from "./directives";
import { ALL_RICH_FEATURES, balanceFences, codeBlockInfo, richKind, type RichFeatures } from "./fence";
import { useHost } from "./host";
import { dataImageOk, LOCAL_IMAGE_SCHEMES, localImageSource, remoteImageLabel, remoteImageUrl } from "./images";
import { RichBlock } from "./RichBlock";

/** Inside a link: an image there is not a second link. */
const InLink = createContext(false);

// `node` is react-markdown's AST handle — strip it so it isn't forwarded onto
// the DOM element.
function Link({ node, ...props }: ComponentProps<"a"> & ExtraProps) {
  void node;
  const { latest } = useHost();
  const own = props.href ? latest.current.link?.(props.href, props.children) : undefined;
  if (own !== undefined && own !== null) return <InLink.Provider value>{own}</InLink.Provider>;
  return (
    <InLink.Provider value>
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
    </InLink.Provider>
  );
}

const ImageIcon = () => (
  <svg className="wf-remote-image-icon" viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false">
    <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
    <circle cx="5.5" cy="6" r="1.2" fill="currentColor" />
    <path d="M2 12l3.5-3.5 2.5 2.5 2-2L14 12.5" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
  </svg>
);

/** A markdown image: drawn, or, where the host keeps remote images from the page, a
 *  link to the one it names (an icon and the alt text, else the host) that
 *  opens as any link does, or the host's own placeholder for it. Its text is
 *  React text, never markup. */
function Image({ node, src, alt, ...props }: ComponentProps<"img"> & ExtraProps) {
  void node;
  const { remoteImages, remoteImageSlot, latest, imageSchemes } = useHost();
  const inLink = useContext(InLink);
  if (typeof src === "string" && src !== "") {
    const own = latest.current.image?.(src, alt ?? "", { inLink });
    if (own !== undefined && own !== null) return <>{own}</>;
    // One of the host's own that it didn't draw: nothing to fetch it from.
    const scheme = schemeOf(src);
    if (scheme && keptSchemes(imageSchemes).includes(scheme)) return <img {...props} alt={alt} />;
  }
  const held = remoteImages === "link" && typeof src === "string" && src !== "" ? src : null;
  const remote = held === null ? null : remoteImageUrl(held);
  if (!remote) {
    // Not remote, yet not a source of the host's own either (an address this
    // cannot read): fail closed, with nothing to fetch.
    if (held !== null && !localImageSource(held)) return <img {...props} alt={alt} />;
    return <img {...props} src={src} alt={alt} loading="lazy" />;
  }
  const label = remoteImageLabel(alt, remote);
  const draw = latest.current.remoteImage;
  if (remoteImageSlot && draw && !inLink) {
    const text = (alt ?? "").replace(/\s+/g, " ").trim();
    return <>{draw({ url: remote.href, site: remote.hostname, alt: text, label })}</>;
  }
  const body = (
    <>
      <ImageIcon />
      <span>{label}</span>
    </>
  );
  // Already inside a link of its own: the one link, not a link in a link.
  if (inLink) return <span className="wf-remote-image">{body}</span>;
  return (
    <Link className="wf-remote-image" href={remote.href} title={remote.hostname}>
      {body}
    </Link>
  );
}

type HastNode = { type: string; value?: string; tagName?: string; properties?: Record<string, unknown>; children?: HastNode[] };

const textOf = (n: HastNode | undefined): string =>
  !n ? "" : n.type === "text" ? (n.value ?? "") : (n.children ?? []).map(textOf).join("");

const prop = (n: HastNode | undefined, name: string): string => {
  const v = n?.properties?.[name];
  return typeof v === "string" ? v : "";
};

const attrsOf = (json: string): Record<string, string> => {
  try {
    const v: unknown = JSON.parse(json);
    if (!v || typeof v !== "object" || Array.isArray(v)) return {};
    return Object.fromEntries(Object.entries(v).filter((e): e is [string, string] => typeof e[1] === "string"));
  } catch {
    return {};
  }
};

/** A directive: the host's drawing of it, else the text it was written as. */
function DirectiveElement({ node, children }: { node?: HastNode; children?: ReactNode }) {
  const { latest } = useHost();
  const kind = prop(node, "dataKind") as DirectiveKind;
  // A container's label is its first child; a text or leaf directive's
  // children are its label.
  const labelNode = kind === "container" ? node?.children?.find((c) => c.type === "element" && c.tagName === DIRECTIVE_LABEL_TAG) : node;
  const all = Children.toArray(children);
  const isLabel = (c: ReactNode) => isValidElement<{ node?: HastNode }>(c) && c.props.node?.tagName === DIRECTIVE_LABEL_TAG;
  const label = kind === "container" ? (all.find(isLabel) ?? null) : all.length ? children : null;
  const content = kind === "container" ? all.filter((c) => !isLabel(c)) : null;
  const own = latest.current.directive?.({
    kind,
    name: prop(node, "dataName"),
    attributes: attrsOf(prop(node, "dataAttrs")),
    label,
    labelText: labelNode && (kind !== "container" || labelNode !== node) ? textOf(labelNode) : "",
    children: content,
  });
  if (own !== undefined && own !== null) return <>{own}</>;
  // Not drawn: as it was written, its label and content drawn as markdown.
  const name = prop(node, "dataName");
  const attrs = attributesText(attrsOf(prop(node, "dataAttrs")));
  const head = (marker: string) => (
    <>
      {marker}
      {name}
      {label !== null && (
        <>
          [{label}]
        </>
      )}
      {attrs}
    </>
  );
  if (kind === "text") return head(":");
  if (kind === "leaf") return <p>{head("::")}</p>;
  const fence = ":".repeat(Number(prop(node, "dataColons")) || 3);
  return (
    <>
      <p>{head(fence)}</p>
      {content}
      {prop(node, "dataClosed") && <p>{fence}</p>}
    </>
  );
}

/** A container's label, drawn inline where the host puts it. */
function DirectiveLabel({ children }: { children?: ReactNode }) {
  return <>{children}</>;
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
  img: Image,
  ...({ [DIRECTIVE_TAG]: DirectiveElement, [DIRECTIVE_LABEL_TAG]: DirectiveLabel } as Components),
};

/** Schemes that run or embed content: never kept, whatever a host declares. */
const NEVER = new Set(["javascript", "vbscript", "data", "file"]);

/** Schemes from a host's comma-joined list that may be kept. */
const keptSchemes = (list: string): string[] =>
  list
    ? list
        .split(",")
        .map((s) => s.trim().toLowerCase().replace(/:$/, ""))
        .filter((s) => /^[a-z][a-z0-9+.-]*$/.test(s) && !NEVER.has(s))
    : [];

const schemeOf = (url: string): string | null => {
  const colon = url.indexOf(":");
  if (colon < 1 || /[/?#]/.test(url.slice(0, colon))) return null;
  return url.slice(0, colon).toLowerCase();
};

/** The sanitizer and URL transform that keep `schemes` (comma-joined) on links,
 *  besides the defaults. An image's `src` keeps `blob:`, `deskfile:` and
 *  `data:` besides the web's, never a declared link scheme (those are places to
 *  go, not files to fetch); a `data:` image is held to a raster type and a size
 *  by the transform, which sees the whole URL. */
function linkPolicy(
  schemes: string,
  imageSchemes: string,
  directives: boolean,
): {
  rehypePlugins: NonNullable<Options["rehypePlugins"]>;
  urlTransform: (url: string, key: string) => string;
} {
  const kept = keptSchemes(schemes);
  const ownImages = keptSchemes(imageSchemes);
  const schema = {
    ...defaultSchema,
    tagNames: [...(defaultSchema.tagNames ?? []), ...(directives ? [DIRECTIVE_TAG, DIRECTIVE_LABEL_TAG] : [])],
    attributes: { ...defaultSchema.attributes, ...(directives ? { [DIRECTIVE_TAG]: [...DIRECTIVE_PROPS] } : {}) },
    protocols: {
      ...defaultSchema.protocols,
      href: [...(defaultSchema.protocols?.href ?? []), ...kept],
      src: [...(defaultSchema.protocols?.src ?? []), ...LOCAL_IMAGE_SCHEMES, ...ownImages, "data"],
    },
  };
  return {
    rehypePlugins: [[rehypeSanitize, schema]],
    urlTransform: (url, key) => {
      const scheme = schemeOf(url);
      if (key === "src") {
        if (scheme === "data") return dataImageOk(url) ? url : "";
        if (scheme && (LOCAL_IMAGE_SCHEMES.includes(scheme) || ownImages.includes(scheme))) return url;
        return defaultUrlTransform(url);
      }
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
  const { schemes, imageSchemes, directives } = useHost();
  const links = useMemo(() => linkPolicy(schemes, imageSchemes, directives !== ""), [schemes, imageSchemes, directives]);
  const remarkPlugins = useMemo<NonNullable<Options["remarkPlugins"]>>(
    () => (directives ? [remarkGfm, [remarkNamedDirectives, directiveNameList(directives)], remarkWfDirectives] : [remarkGfm]),
    [directives],
  );
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
        remarkPlugins={remarkPlugins}
        rehypePlugins={links.rehypePlugins}
        urlTransform={links.urlTransform}
        components={components}
      >
        {balanceFences(text)}
      </Markdown>
    </div>
  );
}
