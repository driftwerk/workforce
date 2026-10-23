/** A viewer's own preferences, kept in the browser; a viewer without storage keeps them for this visit only. */

export function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeStored(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage is unavailable: a private window or a full quota.
  }
}
