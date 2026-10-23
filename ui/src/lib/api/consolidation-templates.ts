import { gateway } from "../api-client";
import type { DefRevision } from "./revisions";

/** Summary row for the list endpoint (matches the backend template summary). */
export interface ConsolidationTemplateDef {
  id: string;
  name: string;
  /** Owner identity; null for system/global definitions. */
  owner: string | null;
  /** The stored word: `"private"` (owner only), `"team"` (anyone in a team
   *  with the owner) or `"global"` (anyone may use). */
  visibility: "private" | "team" | "global";
}

/** One canonical statement line. `header`/`line`/`subtotal`. */
export type TemplateLineKind = "header" | "line" | "subtotal";

export interface TemplateLine {
  label: string;
  kind: TemplateLineKind;
  /** For a subtotal: the canonical line labels it sums. Empty otherwise. */
  components: string[];
}

/**
 * Full template definition (mirrors the Rust `ConsolidationTemplate`).
 * `map` is keyed entity → { sourceAccountLabel → canonicalLineLabel }: an
 * entity's source account rolls up into one canonical line.
 */
export interface ConsolidationTemplate {
  name?: string;
  lines: TemplateLine[];
  map: Record<string, Record<string, string>>;
}

/** `?owner=<id>` query segment, or empty for a system/global definition. */
function ownerQuery(owner?: string | null): string {
  return owner ? `?owner=${encodeURIComponent(owner)}` : "";
}

export const consolidationTemplatesApi = {
  // Member-visible: the caller sees global templates plus their own.
  list: () =>
    gateway.get<{ templates: ConsolidationTemplateDef[] }>(
      "/api/consolidation-templates",
    ),

  // `id` is the bare id; `owner` (a member's identity, omitted for system/global
  // defs) disambiguates which owner's definition to address. Returns the
  // template JSON as a string.
  getSource: (id: string, owner?: string | null) =>
    gateway.get<string>(
      `/api/consolidation-templates/${id}/source${ownerQuery(owner)}`,
    ),

  /** `from` renames (old id deleted after save); `visibility` sets private/team/global.
   *  The body is the template JSON text; the owner is derived server-side. */
  save: (id: string, json: string, from?: string, visibility?: string) => {
    const params = new URLSearchParams();
    if (from && from !== id) params.set("from", from);
    if (visibility) params.set("visibility", visibility);
    const qs = params.toString();
    return gateway.putText<{ saved: string }>(
      `/api/consolidation-templates/${id}${qs ? `?${qs}` : ""}`,
      json,
    );
  },

  del: (id: string) =>
    gateway.del<{ deleted: string }>(`/api/consolidation-templates/${id}`),

  // Revision history (mirrors playbooks; shares the `DefRevision` shape).
  listRevisions: (id: string, owner?: string | null) =>
    gateway.get<{ revisions: DefRevision[] }>(
      `/api/consolidation-templates/${id}/revisions${ownerQuery(owner)}`,
    ),
  getRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.get<DefRevision>(
      `/api/consolidation-templates/${id}/revisions/${rev}${ownerQuery(owner)}`,
    ),
  restoreRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/consolidation-templates/${id}/revisions/${rev}/restore${ownerQuery(owner)}`,
    ),

  /** Parse a previously-staged `.xlsx` artifact into a draft template. */
  parseXlsx: (artifactId: string) =>
    gateway.post<ConsolidationTemplate>(
      "/api/consolidation-templates/parse-xlsx",
      { artifact_id: artifactId },
    ),

  /**
   * Stage an uploaded file and return its `art_…` artifact id, ready to feed to
   * {@link parseXlsx}. Uses the multipart `postFormData` pattern (mirrors
   * `trainingApi.uploadCorpus`); the staging route is a sibling of the feature
   * base that the backend resolves into a staged artifact.
   */
  uploadXlsx: (file: File) => {
    const form = new FormData();
    form.append("file", file, file.name);
    return gateway.postFormData<{ artifact_id: string }>(
      "/api/consolidation-templates/upload",
      form,
    );
  },
};
