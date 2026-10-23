import { gateway } from "../api-client";

export interface Counts {
  agents: number;
  sessions: number;
  memories: number;
  identities: number;
}

export interface AgentPlan {
  source_id: string;
  target_id: string;
  collides: boolean;
  sessions: number;
  memories: number;
  expected_credentials: string[];
}

export interface IdentityResolution {
  source_id: string;
  email?: string;
  resolution: { kind: "matched" | "create"; target_id: string };
}

export interface MergePlan {
  source_install_id: string;
  source_slack_team_id?: string;
  agents: AgentPlan[];
  identities: IdentityResolution[];
  counts: Counts;
}

export interface ImportOptions {
  renames: Record<string, string>;
  identity_overrides: Record<string, string>;
  confirm: boolean;
}

export interface ImportOutcome {
  agents_imported: string[];
  renames_applied: Record<string, string>;
  identities_created: number;
  identities_matched: number;
  sessions_imported: number;
  memories_imported: number;
  memories_skipped: number;
  notes: string[];
}

export const agentTransferApi = {
  exportBundle: (agents: string[]) =>
    gateway.getBlob(
      `/api/transfer/export?agents=${encodeURIComponent(agents.join(","))}`,
    ),

  uploadBundle: (file: File) => {
    const form = new FormData();
    form.append("file", file, file.name);
    return gateway.postFormData<{ artifact_id: string; plan: MergePlan }>(
      "/api/transfer/import",
      form,
    );
  },

  applyImport: (artifactId: string, options: ImportOptions) =>
    gateway.post<ImportOutcome>(
      `/api/transfer/import/${encodeURIComponent(artifactId)}/apply`,
      options,
    ),
};
