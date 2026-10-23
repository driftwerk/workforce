import { gateway } from "../api-client";
import type { DefRevision } from "./revisions";

/** Summary type for the list endpoint (matches the backend playbook summary). */
export interface PlaybookDef {
  id: string;
  name: string;
  description: string;
  steps: number;
  /** Owner identity; omitted/null for system/global definitions. */
  owner?: string | null;
  /** The stored word: `"private"` (owner only), `"team"` (anyone in a team
   *  with the owner) or `"global"` (anyone may run). */
  visibility?: string;
}

// --- Full playbook definition types (mirror Rust PlaybookDefinition from TOML) ---

/** Slack button styling — Block Kit only offers these three. */
export type ButtonStyle = "default" | "primary" | "danger";

export interface PlaybookButton {
  label: string;
  style: ButtonStyle;
  /** Jump to this step id. Exactly one of goto/exit/finish is set. */
  goto?: string;
  exit?: boolean;
  finish?: boolean;
}

export interface PlaybookStep {
  id: string;
  title: string;
  prompt: string;
  artifact?: string;
  buttons: PlaybookButton[];
  /** Whether the default linear Continue edge is shown (defaults true). */
  keepContinue: boolean;
  /** Optional per-step model override — a tier (`"powerful"`) or a
   *  provider-routed tier (`"openrouter:anthropic:powerful"`). */
  model?: string;
}

export interface PlaybookMeta {
  id: string;
  name: string;
  description: string;
}

export interface PlaybookDefinition {
  playbook: PlaybookMeta;
  step: PlaybookStep[];
}

/** `?owner=<id>` query segment, or empty for a system/global definition. */
function ownerQuery(owner?: string | null): string {
  return owner ? `?owner=${encodeURIComponent(owner)}` : "";
}

export const playbooksApi = {
  // Member-visible: the caller sees global playbooks plus their own.
  list: () => gateway.get<{ playbooks: PlaybookDef[] }>("/api/playbooks"),
  // `id` is the bare id; `owner` (a member's identity, omitted for system/global
  // defs) disambiguates which owner's definition to address.
  getSource: (id: string, owner?: string | null) =>
    gateway.get<string>(`/api/playbooks/${id}/source${ownerQuery(owner)}`),
  /** `from` renames (old id deleted after save); `visibility` sets private/team/global;
   *  `owner` addresses the loaded row so an edit updates it rather than forking. */
  save: (
    id: string,
    toml: string,
    from?: string,
    visibility?: string,
    owner?: string | null,
  ) => {
    const params = new URLSearchParams();
    if (from && from !== id) params.set("from", from);
    if (visibility) params.set("visibility", visibility);
    if (owner) params.set("owner", owner);
    const qs = params.toString();
    return gateway.putText<{ saved: string }>(
      `/api/playbooks/${id}${qs ? `?${qs}` : ""}`,
      toml,
    );
  },
  del: (id: string, owner?: string | null) =>
    gateway.del<{ deleted: string }>(`/api/playbooks/${id}${ownerQuery(owner)}`),
  export: () => gateway.get<Record<string, string>>("/api/playbooks/export"),
  // Revision history.
  listRevisions: (id: string, owner?: string | null) =>
    gateway.get<{ revisions: DefRevision[] }>(`/api/playbooks/${id}/revisions${ownerQuery(owner)}`),
  getRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.get<DefRevision>(`/api/playbooks/${id}/revisions/${rev}${ownerQuery(owner)}`),
  restoreRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/playbooks/${id}/revisions/${rev}/restore${ownerQuery(owner)}`,
    ),
};
