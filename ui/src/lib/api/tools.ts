import { gateway } from "../api-client";

export interface ToolProvenance {
  kind: "builtin" | "mcp";
  /** Friendly integration/group name, e.g. "Vault", "Metabase". */
  label: string;
  /** MCP server id, or null for built-ins. */
  server: string | null;
}

export interface ToolInfo {
  name: string;
  description: string;
  /** Present on newer APIs; absent mid-deploy or on older builds. */
  provenance?: ToolProvenance;
}

export interface ToolListResponse {
  total: number;
  tools: ToolInfo[];
}

export const toolsApi = {
  /** Deduped union of every tool across all loaded agents (admin-only). */
  listGlobal: () =>
    gateway.get<ToolListResponse>("/api/tools?scope=global"),
};
