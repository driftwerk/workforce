/** Escape a TOML basic-string value (backslashes and double quotes). */
export function escapeTomlString(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

/** Whether a string is better written as a TOML multi-line (triple-quoted) value. */
export function isMultiLine(s: string): boolean {
  return s.includes("\n") || s.length > 120;
}
