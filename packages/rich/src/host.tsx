// The one seam between the renderers and the app embedding them. Every member
// is optional: a host that cannot page a staged table, fetch a staged file or
// take a form submission leaves it out and the block degrades — a plain notice,
// an unavailable frame, a read-only form — instead of failing.

import { createContext, useContext, useMemo, useRef, type ReactNode, type RefObject } from "react";

import { directiveNameList, type Directive } from "./directives";
import type { SubmitValue, TablePage } from "./spec";

/** A remote markdown image, as {@link RichHost.remoteImage} is given it. */
export interface RemoteImage {
  /** Its absolute `http(s)` address (a protocol-relative one made `https:`). */
  url: string;
  /** The address's host name: the site the picture is on. */
  site: string;
  /** Its alt text, whitespace collapsed; empty when it has none. */
  alt: string;
  /** What a link to it would say: the alt text shortened, else the site. */
  label: string;
}
import type { TableParams } from "./tableParams";

export interface RichHost {
  /** One filtered/sorted page of a staged table; also backs a form field's
   *  `options_ref` typeahead. Reject with {@link TableExpired} once the artifact
   *  is gone, so the block says so instead of offering a retry. */
  fetchTablePage?(artifactId: string, params: TableParams): Promise<TablePage>;
  /** A staged file's bytes, for a `wf-image`. The id is a request, never a
   *  grant: the host must resolve it through its own owner-scoped route. */
  fetchFile?(artifactId: string): Promise<Blob>;
  /** Deliver a completed form to the running conversation: `summary` becomes
   *  the user turn, `values` the exact machine arguments. Resolve `true` once the
   *  turn is on its way, `false` if it was dropped before running — the form only
   *  stays locked when it really was sent. */
  submitForm?(summary: string, values: Record<string, SubmitValue>): Promise<boolean>;
  /** Open a link from rendered markdown. Absent, links open in a new tab. */
  openUrl?(url: string): void;
  /** Draw a link from rendered markdown the host's own way, such as a link to
   *  one of its records drawn as a chip with that record's state. Called with
   *  the link's address after the scheme policy (so only a kept scheme reaches
   *  it) and its rendered text. Return `undefined` or `null` to draw the
   *  ordinary link. What it returns stands inside a paragraph, so it must be
   *  phrasing content, and it replaces the link whole: no `target`, `rel` or
   *  `openUrl`, so it handles its own click. It's called as the markdown
   *  renders, so what it returns should read any changing state itself (a
   *  component that subscribes) rather than from the closure. */
  link?(href: string, children: ReactNode): ReactNode | null | undefined;
  /** Draw a markdown directive (`:mention[Ann]`, `::name`, `:::panel{tone=info}`
   *  … `:::`) the host's own way. Only names in `directiveNames` are read as
   *  directives, and only with this hook: anything else that looks like one
   *  (`10:30`, `Note:this`, `mailto:a@b.c`) stays what it was, words, links
   *  and autolinks alike. Switch on the directive's name (and its kind).
   *  Return `undefined` or `null` for one it doesn't draw: that reads as it
   *  was written, its delimiters around its label and content drawn as
   *  markdown (`:mention[**Ann**]` shows `:mention[` **Ann** `]`). A text
   *  directive stands inside a paragraph, so it must be phrasing content; a
   *  leaf or container directive stands where a block does. Its attributes are
   *  the author's words: draw only values the host knows. */
  directive?(directive: Directive): ReactNode | null | undefined;
  /** The directive names `directive` draws, such as `["mention", "panel"]`;
   *  matched without regard to case. No names, no directives. */
  directiveNames?: readonly string[];
  /** Draw a markdown image the host's own way, such as a file of its own named
   *  by a scheme in `imageSchemes`. Called with the image's source after the
   *  scheme policy, its alt text, and whether it stands inside a link (then
   *  what it returns is that link's content, so nothing interactive: no
   *  button). It's called before `remoteImages` is applied: return `undefined`
   *  or `null` to draw it as any image is. It stands where an inline image
   *  would, often inside a paragraph, so it must be phrasing content. An image
   *  in one of `imageSchemes` it doesn't draw isn't drawn at all: only its alt
   *  text. */
  image?(src: string, alt: string, at: { inLink: boolean }): ReactNode | null | undefined;
  /** Image source schemes of the host's own, kept for `image` to draw. Never
   *  a link's, and never `javascript`, `vbscript`, `data` or `file`. This list
   *  is read without regard to case, a trailing colon ignored; a source in
   *  the markdown must name its scheme in lowercase, as with the web's. */
  imageSchemes?: readonly string[];
  /** Link schemes kept besides the web's (http, https, mailto), opened through
   *  `openUrl`. Every other scheme is stripped from agent markdown. */
  linkSchemes?: readonly string[];
  /** What a markdown image at a remote `http(s)` address becomes. `"render"`
   *  (the default) draws it, so the page fetches it from that address; `"link"`
   *  shows a link to it instead (an image icon and its alt text, else its
   *  host), opened through `openUrl`, and the page fetches nothing. A host
   *  whose content-security policy bars remote images says `"link"`, since
   *  they would only show as broken. Images that come from the host itself
   *  (`blob:`, `deskfile:` or `http(s)://deskfile.localhost`, a small raster
   *  `data:` image, a path on the page's own origin) are drawn either way; in
   *  `"link"` mode any other source is not drawn at all. A declared link scheme
   *  is never an image source. */
  remoteImages?: "link" | "render";
  /** In `"link"` mode, what a remote image is drawn as instead of the link:
   *  a host that fetches pictures itself, once the person asks, shows its own
   *  placeholder here. It stands where an inline image would, often inside a
   *  paragraph, so it must be phrasing content (spans and buttons, no `div`).
   *  An image inside a link is still only that link's text: a button can't go
   *  in a link. Ignored in `"render"` mode. */
  remoteImage?(image: RemoteImage): ReactNode;
  /** The form was answered already, here or before: it shows locked and says so. */
  answered?: boolean;
  /** Save a file the block generated (CSV, PNG, Excalidraw scene). Absent, the
   *  browser's anchor download is used. */
  saveFile?(blob: Blob, filename: string): void;
}

/** The staged artifact is gone (TTL-swept); the block shows an expiry note. */
export class TableExpired extends Error {}

interface HostHandle {
  /** The latest host, read at call time: a host object rebuilt on every render
   *  must not refetch a table or restart a typeahead. */
  latest: RefObject<RichHost>;
  /** Which members are present — the only part of the host effects key on. */
  tables: boolean;
  files: boolean;
  forms: boolean;
  answered: boolean;
  remoteImages: "link" | "render";
  /** Whether the host draws remote images itself (`remoteImage`). */
  remoteImageSlot: boolean;
  /** `linkSchemes`, joined, so a host rebuilt with the same list keeps its renderers. */
  schemes: string;
  /** `imageSchemes`, joined, likewise. */
  imageSchemes: string;
  /** The directive names read, comma-joined: the host's `directiveNames`
   *  when it draws directives (`directive`), else none. */
  directives: string;
}

const NO_HOST: HostHandle = {
  latest: { current: {} },
  tables: false,
  files: false,
  forms: false,
  answered: false,
  remoteImages: "render",
  remoteImageSlot: false,
  schemes: "",
  imageSchemes: "",
  directives: "",
};

const HostContext = createContext<HostHandle>(NO_HOST);

export function RichHostProvider({ host, children }: { host: RichHost; children: ReactNode }) {
  const latest = useRef(host);
  latest.current = host;
  const tables = !!host.fetchTablePage;
  const files = !!host.fetchFile;
  const forms = !!host.submitForm;
  const answered = !!host.answered;
  const remoteImages: HostHandle["remoteImages"] = host.remoteImages === "link" ? "link" : "render";
  const remoteImageSlot = !!host.remoteImage;
  const schemes = (host.linkSchemes ?? []).join(",");
  const imageSchemes = (host.imageSchemes ?? []).join(",");
  const directives = host.directive ? directiveNameList((host.directiveNames ?? []).join(",")).join(",") : "";
  const handle = useMemo(
    () => ({ latest, tables, files, forms, answered, remoteImages, remoteImageSlot, schemes, imageSchemes, directives }),
    [tables, files, forms, answered, remoteImages, remoteImageSlot, schemes, imageSchemes, directives],
  );
  return <HostContext.Provider value={handle}>{children}</HostContext.Provider>;
}

export const useHost = (): HostHandle => useContext(HostContext);

/** Hand a blob to the browser as a download. */
export function anchorDownload(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.click();
  // Revoke after the click's synchronous save-initiation, not before, or the
  // browser can cancel the in-flight download.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** The host's file saver, falling back to the anchor download. */
export function useSaveFile(): (blob: Blob, filename: string) => void {
  const { latest } = useHost();
  return (blob, filename) => (latest.current.saveFile ?? anchorDownload)(blob, filename);
}
