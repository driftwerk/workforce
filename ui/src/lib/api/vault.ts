import { gateway } from "../api-client";

export type VaultTier =
  | "agent_per_user"
  | "global_per_user"
  | "agent_specific"
  | "global";

export const VAULT_TIERS: VaultTier[] = [
  "agent_per_user",
  "global_per_user",
  "agent_specific",
  "global",
];

export interface VaultEntryView {
  path: string;
  tier: VaultTier;
  fingerprint: string;
  created_at: string;
  updated_at: string;
  created_by: string;
  updated_by: string;
  created_by_display?: string;
  updated_by_display?: string;
  /** Present for per-agent tiers (agent_per_user / agent_specific). */
  agent_id?: string;
}

export interface VaultListResponse {
  keys: string[];
  entries: VaultEntryView[];
}

export interface VaultMoveRequest {
  path: string;
  from_tier: VaultTier;
  to_tier: VaultTier;
  /** Optional destination path (rename). Omit to keep the path and only change tier. */
  to_path?: string;
  user_id?: string;
  overwrite?: boolean;
}

export interface VaultMoveResponse {
  moved: string;
  to_path?: string;
  from: VaultTier;
  to: VaultTier;
}

export interface VaultCopyRequest {
  path: string;
  from_tier: VaultTier;
  to_tier: VaultTier;
  /** Source user — required when from_tier is per-user. */
  user_id?: string;
  to_user_ids: string[];
  overwrite?: boolean;
}

export type VaultCopyStatus = "copied" | "exists" | "source" | "error";

export interface VaultCopyOutcome {
  user_id: string;
  status: VaultCopyStatus;
  error?: string;
}

export interface VaultCopyResponse {
  agent_id: string;
  path: string;
  from_tier: VaultTier;
  to_tier: VaultTier;
  source_user: string | null;
  copied: number;
  results: VaultCopyOutcome[];
}

// ── Credential catalog (vault editor path dropdown) ─────────────────────────

/** A `{var}` segment in a catalog template — the "Change-Me" the user fills. */
export interface CatalogVariable {
  name: string;
  label: string;
  placeholder: string;
  example?: string;
}

export type CredScope = "per_user" | "shared" | "either";
export type CredManaged = "manual" | "oauth";

/** One offerable credential path for the path dropdown. */
export interface CatalogEntry {
  id: string;
  skill?: string;
  group: string;
  label: string;
  /** Path template with `{var}` placeholders. */
  template: string;
  /** Absent in older responses for fixed (non-templated) paths. */
  variables?: CatalogVariable[];
  /** Origin of the credential. Absent in older responses → treat as "builtin". */
  source?: "builtin" | "mcp";
  /** Whether offered in the add dropdown. Absent → true; runtime OAuth → false. */
  addable?: boolean;
  scope: CredScope;
  managed_by: CredManaged;
  /** Whether a non-admin per-user write is permitted (server gate). */
  per_user_allowed: boolean;
  /** The value is a whole file, so the editor shows a text area. Absent → false. */
  multiline?: boolean;
  required: boolean;
  description: string;
}

export interface CatalogResponse {
  entries: CatalogEntry[];
}

export const vaultApi = {
  list: (prefix?: string) => {
    const params = prefix ? `?prefix=${encodeURIComponent(prefix)}` : "";
    return gateway.get<VaultListResponse>(`/api/vault${params}`);
  },
  get: (path: string) =>
    gateway.get<{ path: string; value: string }>(`/api/vault/${path}`),
  set: (path: string, value: string) =>
    gateway.post<{ stored: string }>(`/api/vault/${path}`, { value }),
  del: (path: string) => gateway.del<{ deleted: string }>(`/api/vault/${path}`),
  rename: (from: string, to: string) =>
    gateway.post<{ renamed: string; to: string }>(`/api/vault/rename`, { from, to }),
  catalog: () => gateway.get<CatalogResponse>(`/api/vault/catalog`),
  /** Catalog entries for installed MCP servers (preset + custom). Admin-only;
   * callers without admin scope (e.g. My Settings) get 403 and should ignore. */
  mcpCatalog: () =>
    gateway.get<CatalogResponse>(`/api/mcp/credential-catalog`),
};

/**
 * Fill a catalog template's `{var}` segments from a values map, producing the
 * concrete vault path. Unfilled variables are left as their `{name}` token so
 * the caller can detect an incomplete path.
 */
export function resolveTemplate(
  template: string,
  values: Record<string, string>,
): string {
  return template.replace(/\{(\w+)\}/g, (_m, name: string) =>
    values[name]?.trim() ? values[name].trim() : `{${name}}`,
  );
}

/** True when a resolved path still contains an unfilled `{var}` token. */
export function hasUnfilledVariables(path: string): boolean {
  return /\{[a-zA-Z_]\w*\}/.test(path);
}

/**
 * Match a path against a template with at most one `{var}` segment. Returns the
 * extracted variable values when it matches (an empty map for fixed templates),
 * or `null` when it doesn't. The variable matches exactly one non-empty segment.
 */
export function matchTemplate(
  template: string,
  path: string,
): Record<string, string> | null {
  const open = template.indexOf("{");
  if (open === -1) return template === path ? {} : null;
  const close = template.indexOf("}", open);
  if (close === -1) return template === path ? {} : null;
  const name = template.slice(open + 1, close);
  const prefix = template.slice(0, open);
  const suffix = template.slice(close + 1);
  if (!path.startsWith(prefix) || !path.endsWith(suffix)) return null;
  const v = path.slice(prefix.length, path.length - suffix.length);
  if (!v || v.includes("/") || v.includes(".")) return null;
  return { [name]: v };
}

/** How a stored path relates to the catalog: where it groups + whether it's recognised. */
export interface PathClass {
  /** Top-level group (skill / family / MCP server id) for the grouped view. */
  group: string;
  /** The matched `{var}` value (workspace / instance / account / …), or null. */
  configuration: string | null;
  source: "builtin" | "mcp" | null;
  managed_by: CredManaged | null;
  /** True when the path matches a catalog template. */
  recognised: boolean;
  /** True when the matched entry may only live in a shared tier, so a
   * per-user copy is ignored by every loader. */
  sharedOnly: boolean;
  /** True when the path sits in a catalogued namespace but matches nothing —
   * the "unrecognised" badge. Paths outside every catalogued namespace are
   * left unbadged so the signal stays trustworthy. */
  orphan: boolean;
}

/** Top-level group + source for a path that matched no template, derived from
 * its own shape so custom/stale rows still nest sensibly instead of in "Other". */
function deriveGroup(path: string): { group: string; source: "mcp" | null } {
  if (path.startsWith("mcp/") || path.startsWith("mcp.")) {
    const segs = path.split("/");
    const id = segs[1] === "v1" ? segs[2] : segs[1];
    return { group: id || "mcp", source: "mcp" };
  }
  if (path.startsWith("skills/")) {
    return { group: path.split("/")[1] || "Other", source: null };
  }
  const head = path.split(/[/.]/)[0];
  return { group: head || "Other", source: null };
}

/**
 * Build a classifier over a catalog: precomputes the catalogued namespaces once,
 * then maps each path to its group/configuration/source and orphan status. This
 * is the single source of truth for both the grouped view and the badge.
 */
export function makePathClassifier(
  catalog: CatalogEntry[],
): (path: string) => PathClass {
  const namespaces = new Set<string>();
  for (const e of catalog) {
    const seps = [e.template.indexOf("/"), e.template.indexOf(".")].filter(
      (i) => i >= 0,
    );
    if (seps.length === 0) continue;
    namespaces.add(e.template.slice(0, Math.min(...seps) + 1));
  }
  return (path) => {
    for (const e of catalog) {
      const vars = matchTemplate(e.template, path);
      if (vars) {
        const values = Object.values(vars);
        return {
          group: e.group,
          configuration: values[0] ?? null,
          source: e.source ?? "builtin",
          managed_by: e.managed_by,
          recognised: true,
          sharedOnly: !e.per_user_allowed,
          orphan: false,
        };
      }
    }
    const derived = deriveGroup(path);
    return {
      group: derived.group,
      configuration: null,
      source: derived.source,
      managed_by: null,
      recognised: false,
      sharedOnly: false,
      orphan: [...namespaces].some((ns) => path.startsWith(ns)),
    };
  };
}

/** Owner uuid of an entry (its `created_by` is `user:<uuid>` for per-user tiers). */
export function vaultEntryOwnerId(entry: VaultEntryView): string {
  return entry.created_by.startsWith("user:")
    ? entry.created_by.slice("user:".length)
    : entry.created_by;
}
