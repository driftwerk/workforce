import { gateway } from "../api-client";

export interface McpServerSummary {
  id: string;
  endpoint: string;
  auth_kind: string;
  tool_count: number;
  installed_via: string;
}

export interface McpSetupStep {
  title: string;
  instructions: string;
  url?: string;
  vault_path?: string;
}

export interface McpPresetSummary {
  id: string;
  label: string;
  endpoint: string;
  auth_kind: "pat" | "oauth2" | "oauth2+pat" | "oauth2_dynamic";
  required_vault_paths: string[];
  oauth_app_registration_url: string | null;
  /// Suggested value for `mcp/v1/<id>/redirect_uri`. `null` when
  /// `[hub].public_url` isn't configured — UI should prompt the
  /// operator to set it.
  suggested_redirect_uri: string | null;
  /// Hand-curated walkthrough — every step the operator needs to
  /// take to go from "nothing" to a working install.
  setup_steps: McpSetupStep[];
}

export type McpInstallReport =
  | {
      status: "installed";
      server_id: string;
      tool_count: number;
      registered_tools: string[];
      auth_kind: string;
    }
  | {
      status: "needs_credentials";
      server_id: string;
      missing: string[];
      instructions: string;
      setup_steps?: McpSetupStep[];
    };

export interface McpUninstallReport {
  server_id: string;
  removed_tools: string[];
  purged_tokens: boolean;
}

export interface McpAuthorizeStart {
  server_id: string;
  provider: string;
  authorization_url: string;
  expires_in_secs: number;
}

export interface McpRefreshReport {
  server_id: string;
  tool_count: number;
}

export interface McpInstallRequest {
  preset?: string;
  custom?: {
    endpoint: string;
    transport?: string;
    auth: unknown;
    allowed_hosts?: string[];
    refresh_interval_secs?: number;
    shadows?: string[];
    fallback_to_builtin?: boolean;
  };
  server_id?: string;
  /// When the calling user has no token for this MCP server, fall back
  /// to the shadowed built-in skill. Omit to keep the preset / custom
  /// default of `false`.
  fallback_to_builtin?: boolean;
}

export const mcpApi = {
  listServers: () =>
    gateway.get<{ servers: McpServerSummary[] }>("/api/mcp/servers"),
  listPresets: () =>
    gateway.get<{ presets: McpPresetSummary[] }>("/api/mcp/presets"),
  install: (req: McpInstallRequest) =>
    gateway.post<McpInstallReport>("/api/mcp/servers", req),
  uninstall: (id: string, purge = false) =>
    gateway.del<McpUninstallReport>(
      `/api/mcp/servers/${encodeURIComponent(id)}${purge ? "?purge=true" : ""}`,
    ),
  refresh: (id: string) =>
    gateway.post<McpRefreshReport>(
      `/api/mcp/servers/${encodeURIComponent(id)}/refresh`,
    ),
  authorize: (id: string) =>
    gateway.post<McpAuthorizeStart>(
      `/api/mcp/servers/${encodeURIComponent(id)}/authorize`,
    ),
};
