import { gateway } from "../api-client";
import type { DefRevision } from "./revisions";
import type { Finding } from "@/lib/subscriptions/findings";

export interface ConfigSaved {
  saved: boolean;
  restart_required: string[];
  findings: Finding[];
  etag: string;
}

export interface AgentInfo {
  id: string;
  name: string | null;
  model: string | null;
  capabilities: string[];
  default: boolean;
  owner_id?: string | null;
  owner_type?: string | null;
}

export interface AgentDetail extends AgentInfo {
  identity_files?: string[];
  tool_count?: number;
  session_count?: number;
  owner_id?: string | null;
  owner_type?: string | null;
}

/** One of an agent's Slack workspaces, as `GET /api/agents/{id}/slack` reports it. */
export interface SlackWorkspaceStatus {
  workspace_id: string;
  connected: boolean;
  bot_name: string | null;
  bot_user_id: string | null;
  /** Why the workspace is not connected. */
  reason: string | null;
}

export interface AgentSlackStatus {
  agent_id: string;
  workspaces: SlackWorkspaceStatus[];
}

/** The tokens of a Slack app made for one agent. */
export interface ConnectSlackRequest {
  bot_token: string;
  app_token: string;
  signing_secret?: string;
  workspace_id?: string;
}

export interface SlackConnection {
  workspace_id: string;
  agent_id: string;
  bot_name: string;
  bot_user_id: string;
  team_id: string | null;
  listening_here: boolean;
  unconnected: Record<string, string>;
}

export interface CreateAgentRequest {
  id: string;
  name?: string;
  model?: string;
  default?: boolean;
  owner_id?: string;
  owner_type?: string;
  /** shared (default) = recalls the user's memories from their other agents;
   *  isolated = starts fresh, recalls only what this agent learns itself. */
  agent_recall_scope?: "shared" | "isolated";
}

export interface UpdateAgentRequest {
  name?: string;
  model?: string;
  default?: boolean;
  owner_id?: string;
  owner_type?: string;
}

export interface AgentTool {
  name: string;
  description: string;
  schema?: Record<string, unknown>;
}

export interface AgentStats {
  period: string;
  sessions: number;
  messages: number;
  tokens_in: number;
  tokens_out: number;
}

export interface AgentSkill {
  name: string;
  dir_name?: string;
  description?: string;
  type?: "builtin" | "external";
  editable?: boolean;
  available?: boolean;
  has_config?: boolean;
}

export const agentsApi = {
  list: () => gateway.get<{ agents: AgentInfo[] }>("/api/agents"),
  getDetail: (id: string) => gateway.get<AgentDetail>(`/api/agents/${id}`),
  create: (req: CreateAgentRequest) => gateway.post<AgentDetail>("/api/agents", req),
  update: (id: string, req: UpdateAgentRequest) => gateway.put<AgentDetail>(`/api/agents/${id}`, req),
  delete: (id: string) => gateway.del(`/api/agents/${id}`),

  // Identity files
  getIdentity: (id: string, file: string) =>
    gateway.get<string>(`/api/agents/${id}/identity/${file}`),
  putIdentity: (id: string, file: string, content: string) =>
    gateway.putText(`/api/agents/${id}/identity/${file}`, content),
  // Identity revision history.
  listIdentityRevisions: (id: string, file: string) =>
    gateway.get<{ revisions: DefRevision[] }>(`/api/agents/${id}/identity/${file}/revisions`),
  getIdentityRevision: (id: string, file: string, rev: number) =>
    gateway.get<DefRevision>(`/api/agents/${id}/identity/${file}/revisions/${rev}`),
  restoreIdentityRevision: (id: string, file: string, rev: number) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/agents/${id}/identity/${file}/revisions/${rev}/restore`,
    ),

  // Config (TOML)
  getConfig: (id: string) => gateway.getTagged(`/api/agents/${id}/config`),
  putConfig: (id: string, toml: string, etag: string | null) =>
    gateway.putText<ConfigSaved>(
      `/api/agents/${id}/config`,
      toml,
      etag ? { "If-Match": etag } : {},
    ),
  checkConfig: (id: string, toml: string) =>
    gateway.postText<{ findings: Finding[] }>(`/api/agents/${id}/config/check`, toml),
  // Config revision history.
  listConfigRevisions: (id: string) =>
    gateway.get<{ revisions: DefRevision[] }>(`/api/agents/${id}/config/revisions`),
  getConfigRevision: (id: string, rev: number) =>
    gateway.get<DefRevision>(`/api/agents/${id}/config/revisions/${rev}`),
  restoreConfigRevision: (id: string, rev: number) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/agents/${id}/config/revisions/${rev}/restore`,
    ),

  // Tools
  getTools: (id: string) => gateway.get<{ tools: AgentTool[] }>(`/api/agents/${id}/tools`),

  // Stats
  getStats: (id: string, period: string) =>
    gateway.get<AgentStats>(`/api/agents/${id}/stats?period=${period}`),

  // Vault (per-agent)
  listVault: (id: string) =>
    gateway.get<import("./vault").VaultListResponse>(`/api/agents/${id}/vault`),
  listVaultKeys: (id: string) =>
    gateway.get<{ keys: string[]; entries: import("./vault").VaultEntryView[] }>(
      `/api/agents/${id}/vault`,
    ),
  setVaultSecret: (id: string, path: string, value: string) =>
    gateway.post<{ stored: string }>(`/api/agents/${id}/vault/${path}`, { value }),
  deleteVaultSecret: (id: string, path: string) =>
    gateway.del<{ deleted: string }>(`/api/agents/${id}/vault/${path}`),
  moveVault: (id: string, req: import("./vault").VaultMoveRequest) =>
    gateway.post<import("./vault").VaultMoveResponse>(
      `/api/agents/${id}/vault/move`,
      req,
    ),
  copyVault: (id: string, req: import("./vault").VaultCopyRequest) =>
    gateway.post<import("./vault").VaultCopyResponse>(
      `/api/agents/${id}/vault/copy`,
      req,
    ),

  // Skills
  listSkills: (id: string) =>
    gateway.get<{ skills: AgentSkill[] }>(`/api/agents/${id}/skills`),
  getSkill: (id: string, name: string) =>
    gateway.get<string>(`/api/agents/${id}/skills/${name}`),
  putSkill: (id: string, name: string, content: string) =>
    gateway.putText(`/api/agents/${id}/skills/${name}`, content),
  deleteSkill: (id: string, name: string) =>
    gateway.del(`/api/agents/${id}/skills/${name}`),

  // Slack
  slackStatus: (id: string) => gateway.get<AgentSlackStatus>(`/api/agents/${id}/slack`),
  connectSlack: (id: string, req: ConnectSlackRequest) =>
    gateway.put<SlackConnection>(`/api/agents/${id}/slack`, req),
  disconnectSlack: (id: string) =>
    gateway.del<{ agent_id: string; workspaces: string[] }>(`/api/agents/${id}/slack`),

  // Skill config (TOML)
  getSkillConfig: (id: string, name: string) =>
    gateway.get<string>(`/api/agents/${id}/skills/${name}/config`),
  putSkillConfig: (id: string, name: string, content: string) =>
    gateway.putText(`/api/agents/${id}/skills/${name}/config`, content),
};
