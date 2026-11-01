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

/** Hosts a page serves its own files from over http(s): a webview that maps a
 *  custom scheme to `http://<scheme>.localhost` (Windows) serves `deskfile:`
 *  there. Nothing else on this computer is the host's own: a loopback address
 *  or another `*.localhost` name is some other program, and an image is a GET
 *  the page would send it, so those are links like any remote address. */
const LOCAL_IMAGE_HOSTS: readonly string[] = ["deskfile.localhost"];

/** The scheme of `url`, lowercased; none when it is relative. */
const schemeOf = (url: string): string | null => {
  const colon = url.indexOf(":");
  if (colon < 1 || /[/?#\\]/.test(url.slice(0, colon))) return null;
  return url.slice(0, colon).toLowerCase();
};

/** The address a remote image would be fetched from: an absolute `http(s)` URL
 *  or a protocol-relative one (which the browser reads as http(s)). None for
 *  anything else, and for the host's own file host (`LOCAL_IMAGE_HOSTS`). */
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
  return LOCAL_IMAGE_HOSTS.includes(url.hostname.toLowerCase()) ? null : url;
}

/** Whether a host that keeps remote images from the page may still draw `src`:
 *  one of its own sources (`blob:`, `deskfile:`, its file host), a raster
 *  `data:` image, or a path on the page's own origin. Anything else is not
 *  drawn, so an address this cannot read is never fetched: it fails closed. */
export function localImageSource(src: string): boolean {
  const raw = src.trim();
  if (/^[/\\]{2}/.test(raw)) return false;
  const scheme = schemeOf(raw);
  if (scheme === null) return true;
  if (LOCAL_IMAGE_SCHEMES.includes(scheme)) return true;
  if (scheme === "data") return dataImageOk(raw);
  if (scheme !== "http" && scheme !== "https") return false;
  try {
    return LOCAL_IMAGE_HOSTS.includes(new URL(raw).hostname.toLowerCase());
  } catch {
    return false;
  }
}

/** What the link for a remote image says: its alt text, else the host. */
export function remoteImageLabel(alt: string | undefined, url: URL): string {
  const text = (alt ?? "").replace(/\s+/g, " ").trim();
  const label = text.length > 80 ? `${text.slice(0, 79)}…` : text;
  return label || url.hostname;
}
