import { gateway } from "../api-client";
import type { VaultEntryView } from "./vault";

// ── Types ─────────────────────────────────────────────────────────────────

export interface IntegrationStatus {
  configured: boolean;
  /** "personal" = user has their own key, "shared" = using global, "none" = not configured */
  scope: "personal" | "shared" | "none";
  email?: string;
}

export interface IntegrationsResponse {
  identity: { id: string; display_name: string; email?: string };
  integrations: Record<string, IntegrationStatus>;
}

export interface VaultListResponse {
  paths: string[];
  entries: VaultEntryView[];
}

export interface RemoteMcpToken {
  id: string;
  label: string;
  token_prefix: string;
  created_at: string;
  expires_at: string;
  last_used_at: string | null;
  expired: boolean;
}

export interface RemoteMcpTokensResponse {
  tokens: RemoteMcpToken[];
}

/** The plaintext token is returned exactly once, at creation. */
export interface CreatedRemoteMcpToken {
  id: string;
  label: string;
  token: string;
  expires_at: string;
  connect: { claude_code: string; workforce_local: string };
}

// ── API Client ────────────────────────────────────────────────────────────

export const mySettingsApi = {
  /** GET /api/me/integrations — get status of all user-configurable integrations. */
  getIntegrations: () =>
    gateway.get<IntegrationsResponse>("/api/me/integrations"),

  /** POST /api/me/vault/{path} — set a per-user secret. */
  setVault: (path: string, value: string) =>
    gateway.post<{ stored: string }>(
      `/api/me/vault/${encodeURIComponent(path)}`,
      { value },
    ),

  /** DELETE /api/me/vault/{path} — delete a per-user secret. */
  deleteVault: (path: string) =>
    gateway.del<{ deleted: string }>(
      `/api/me/vault/${encodeURIComponent(path)}`,
    ),

  /** GET /api/me/vault — list all per-user secret keys. */
  listVault: () => gateway.get<VaultListResponse>("/api/me/vault"),

  /** POST /api/me/vault/move — move a per-user credential between tiers.
   *  Per-user tiers only (agent_per_user ↔ global_per_user). When either tier
   *  is `agent_per_user`, pass `agent_id`. */
  moveVault: (req: MoveVaultRequest) =>
    gateway.post<MoveVaultResponse>("/api/me/vault/move", req),

  /** POST /api/me/secret — add a per-user secret at any key, in a per-user tier
   *  only (never shared). `agent_id` required when `tier` is `agent_per_user`. */
  setSecret: (req: SetSecretRequest) =>
    gateway.post<{ stored: string }>("/api/me/secret", req),

  /** DELETE /api/me/secret — delete a per-user secret at any key. Mirror of
   *  setSecret: per-user tier only, `agent_id` required for `agent_per_user`. */
  deleteSecret: (req: DeleteSecretRequest) =>
    gateway.del<{ deleted: string }>(
      `/api/me/secret?${new URLSearchParams({
        path: req.path,
        tier: req.tier,
        ...(req.agent_id ? { agent_id: req.agent_id } : {}),
      }).toString()}`,
    ),

  /** GET /api/me/remote-mcp-tokens — 404 when remote MCP is disabled on this install. */
  listRemoteMcpTokens: () =>
    gateway.get<RemoteMcpTokensResponse>("/api/me/remote-mcp-tokens"),

  /** POST /api/me/remote-mcp-tokens — mint a token; the plaintext is shown once. */
  createRemoteMcpToken: (label: string) =>
    gateway.post<CreatedRemoteMcpToken>("/api/me/remote-mcp-tokens", { label }),

  /** DELETE /api/me/remote-mcp-tokens/{id} — revoke one of the caller's tokens. */
  revokeRemoteMcpToken: (id: string) =>
    gateway.del<void>(`/api/me/remote-mcp-tokens/${encodeURIComponent(id)}`),
};

export interface SetSecretRequest {
  path: string;
  value: string;
  tier: "global_per_user" | "agent_per_user";
  agent_id?: string;
}

export interface DeleteSecretRequest {
  path: string;
  tier: "global_per_user" | "agent_per_user";
  agent_id?: string;
}

export interface MoveVaultRequest {
  path: string;
  from_tier: "agent_per_user" | "global_per_user";
  to_tier: "agent_per_user" | "global_per_user";
  agent_id?: string;
  overwrite?: boolean;
}

export interface MoveVaultResponse {
  moved: string;
  from: string;
  to: string;
  agent_id?: string;
}
