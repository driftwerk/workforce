/**
 * Where to land after a sign-in that leaves the page — SSO bounces through
 * the identity provider and comes back to `/`, so the page that asked for
 * sign-in (e.g. `/pair?code=…`) is parked here first and restored after.
 *
 * Only same-origin relative paths are kept: anything that could name
 * another origin (`//host`, `https://…`, `/\host`) is dropped, so the
 * parked value can never become an open redirect.
 */
export const RETURN_TO_KEY = "workforce-return-to";

/** `path` when it is a safe same-origin relative path, else `null`. */
export function safeReturnTo(path: string | null | undefined): string | null {
  if (!path) return null;
  if (!path.startsWith("/")) return null;
  if (path.startsWith("//") || path.startsWith("/\\")) return null;
  // Control characters (including tab/newline, which URL parsing strips) can
  // smuggle a second slash past the check above.
  for (let i = 0; i < path.length; i++) {
    if (path.charCodeAt(i) < 0x20 || path.charCodeAt(i) === 0x7f) return null;
  }
  // A bare `/` is where the callback lands anyway; nothing to restore.
  if (path === "/") return null;
  return path;
}

/** Park `path` (pathname + search) to come back to after sign-in. */
export function saveReturnTo(path: string): void {
  const safe = safeReturnTo(path);
  try {
    if (safe) sessionStorage.setItem(RETURN_TO_KEY, safe);
    else sessionStorage.removeItem(RETURN_TO_KEY);
  } catch {
    /* storage unavailable — sign-in still works, it just lands on `/` */
  }
}

/** Take (read and clear) the parked path, if it is still safe. */
export function takeReturnTo(): string | null {
  try {
    const parked = sessionStorage.getItem(RETURN_TO_KEY);
    sessionStorage.removeItem(RETURN_TO_KEY);
    return safeReturnTo(parked);
  } catch {
    return null;
  }
}
