/** What the hub makes of an agent's LLM config, against the spec (or
 *  `[llm.logins]`) it concerns. */
export interface Finding {
  severity: "refused" | "warning";
  spec: string;
  message: string;
}

/** The findings about a field, array item or map value holding `value`. */
export function attachFindings(findings: readonly Finding[], value: string): Finding[] {
  const text = value.trim();
  return text ? findings.filter((finding) => finding.spec.trim() === text) : [];
}

/** The findings no value on the form holds, shown above it. */
export function unattachedFindings(
  findings: readonly Finding[],
  values: readonly string[],
): Finding[] {
  const shown = new Set(values.map((value) => value.trim()).filter(Boolean));
  return findings.filter((finding) => !shown.has(finding.spec.trim()));
}

/** Whether a config the hub would refuse is on the form. */
export function blocksSave(findings: readonly Finding[]): boolean {
  return findings.some((finding) => finding.severity === "refused");
}

/** The findings a refused write carried in its `{error, findings}` body. */
export function refusalFindings(error: unknown): Finding[] {
  if (!(error instanceof Error)) return [];
  try {
    const body = JSON.parse(error.message);
    return Array.isArray(body?.findings) ? (body.findings as Finding[]) : [];
  } catch {
    return [];
  }
}
