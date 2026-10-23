import { gateway } from "../api-client";

/** Skill name → its instances in order; the first is the skill's default inside the environment. */
export type SkillBindings = Record<string, string[]>;

/** MCP workspace → the one variant the environment pins it to. */
export type McpBindings = Record<string, string>;

export interface EnvironmentView {
  name: string;
  default: boolean;
  /** Made by the agent itself; no session uses it until a person approves it. */
  draft: boolean;
  description: string | null;
  skills: SkillBindings;
  mcp: McpBindings;
  warnings: string[];
}

export interface SkillOption {
  name: string;
  instances: string[];
  default: string;
}

export interface McpWorkspaceOption {
  name: string;
  envs: string[];
}

export interface EnvironmentOptions {
  skills: SkillOption[];
  mcp_workspaces: McpWorkspaceOption[];
}

/** One agent's environments. `allowed` is false when the install's kill switch turned them off. */
export interface AgentEnvironments {
  allowed: boolean;
  enabled: boolean;
  default: string | null;
  environments: EnvironmentView[];
  options: EnvironmentOptions;
  etag: string;
  /** Returned by a save: MCP servers a binding names that are installed but not connected. */
  notes?: string[];
}

export interface EnvironmentDef {
  description?: string;
  skills?: SkillBindings;
  mcp?: McpBindings;
  draft?: boolean;
}

/** The agent's whole environments table, as PUT replaces it. */
export interface EnvironmentsBody {
  enabled: boolean;
  default: string | null;
  defs: Record<string, EnvironmentDef>;
}

const path = (agentId: string) => `/api/agents/${encodeURIComponent(agentId)}/environments`;

export const environmentsApi = {
  get: (agentId: string) => gateway.get<AgentEnvironments>(path(agentId)),
  put: (agentId: string, etag: string, body: EnvironmentsBody) =>
    gateway.put<AgentEnvironments>(path(agentId), body, { "If-Match": etag }),
};
