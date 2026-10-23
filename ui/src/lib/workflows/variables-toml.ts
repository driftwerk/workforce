import { stringify } from "smol-toml";
import type { VariableDef } from "@/lib/api/workflows";

/**
 * Replace the `[[variables]]` section of a workflow TOML with `variables`,
 * preserving every other line verbatim. The structured variables editor edits
 * only this block, so the rest of the document (steps with fields the visual
 * parser doesn't model, comments, formatting) is left untouched. Passing an
 * empty list removes the section.
 */
export function upsertVariablesBlock(toml: string, variables: VariableDef[]): string {
  const base = stripVariablesBlocks(toml);
  if (variables.length === 0) return `${base}\n`;
  const clean = variables.map((v) => {
    const o: Record<string, unknown> = { id: v.id };
    if (v.type) o.type = v.type;
    o.value = v.value;
    if (v.allowed_hosts && v.allowed_hosts.length > 0) o.allowed_hosts = v.allowed_hosts;
    return o;
  });
  const block = stringify({ variables: clean }).trimEnd();
  return `${base}\n\n${block}\n`;
}

/** Remove every top-level `[[variables]]` array-of-table block from a TOML string. */
function stripVariablesBlocks(toml: string): string {
  const out: string[] = [];
  let skip = false;
  for (const line of toml.split("\n")) {
    const t = line.trim();
    if (t === "[[variables]]") {
      skip = true;
      continue;
    }
    // Any other table header ends a variables block.
    if (skip && t.startsWith("[")) skip = false;
    if (!skip) out.push(line);
  }
  return out.join("\n").replace(/\n{3,}/g, "\n\n").trimEnd();
}

const SCOPE_RE = /^\[\[vault:(global_user|agent_user|global|agent):([^\]]+)\]\]$/;

/** Parse a secret variable's value into `(scope, key)`, or `null` if it isn't a marker. */
export function parseSecretMarker(value: string): { scope: string; key: string } | null {
  const m = value.match(SCOPE_RE);
  return m ? { scope: m[1]!, key: m[2]! } : null;
}

/** Build a scoped vault marker from a scope + key. */
export function buildSecretMarker(scope: string, key: string): string {
  return `[[vault:${scope}:${key}]]`;
}

export const VAULT_SCOPES = ["global_user", "agent_user", "global", "agent"] as const;
