/**
 * One row of definition revision history (matches the backend `DefRevision`
 * from `def_revisions` — workflows, playbooks, and agent identity files all
 * share the shape). `content` is only present on the single-revision
 * endpoint; list responses carry metadata only.
 */
export interface DefRevision {
  rev: number;
  content?: string;
  content_hash: string;
  /** Delete tombstone — records who deleted the definition. Not restorable. */
  deleted: boolean;
  /** Authenticated identity, or null for unattributed (seed/oob) captures. */
  edited_by: string | null;
  /** web | slack | cli | agent_tool | restore | oob | seed */
  edited_via: string;
  /** Source revision when edited_via === "restore". */
  restored_from: number | null;
  created_at: string;
}
