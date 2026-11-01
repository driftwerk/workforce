// Which markdown images are drawn and which become links. Agent markdown names
// image sources freely, and the page that draws one fetches it: from wherever
// the agent (or whoever fed it text) pointed. A host that cannot or will not do
// that says `remoteImages: "link"`, and a remote image is a link to it instead.

/** The largest `data:` image kept, as the text of its URL (about 750 KB of
 *  picture): a message is not a place to carry a file. */
export const MAX_DATA_IMAGE = 1024 * 1024;

/** A `data:` image the page may draw: a raster format, base64. Never SVG, which
 *  can carry script and external references. */
const DATA_IMAGE = /^data:image\/(?:png|jpe?g|gif|webp);base64,[A-Za-z0-9+/]+={0,2}$/i;

/** Schemes whose images come from the host itself, never fetched from a
 *  remote address: kept for an image wherever the host declared them. */
export const LOCAL_IMAGE_SCHEMES: readonly string[] = ["blob", "deskfile"];

export function dataImageOk(url: string): boolean {
  return url.length <= MAX_DATA_IMAGE && DATA_IMAGE.test(url);
}

/** The address a remote image would be fetched from: an absolute `http(s)` URL
 *  or a protocol-relative one (which the browser reads as http(s)). None for
 *  anything else, and for an address on this computer (loopback and
 *  `*.localhost`, the names a host serves its own files from), which is no
 *  remote fetch. */
export function remoteImageUrl(src: string | undefined): URL | null {
  if (!src) return null;
  const raw = src.trim();
  let url: URL;
  try {
    // A browser reads `\` as `/` in an http URL, so `/\host/x` is `//host/x`.
    url = /^[/\\]{2}/.test(raw) ? new URL(`https:${raw.replace(/\\/g, "/")}`) : new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  const host = url.hostname.toLowerCase();
  const local =
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host === "[::1]" ||
    /^127(?:\.\d{1,3}){3}$/.test(host);
  return local ? null : url;
}

/** What the link for a remote image says: its alt text, else the host. */
export function remoteImageLabel(alt: string | undefined, url: URL): string {
  const text = (alt ?? "").replace(/\s+/g, " ").trim();
  const label = text.length > 80 ? `${text.slice(0, 79)}…` : text;
  return label || url.hostname;
}
