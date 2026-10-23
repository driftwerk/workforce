import { gateway } from "../api-client";

export type PermissionState = "allow" | "ask" | "always_ask" | "deny";
export type PermissionScope = "team" | "agent" | "user";
export type PermissionOrigin = "admin" | "user_runtime_choice" | "proposal";

export type ApproverRole = "system_admin" | "manager" | "member";

export type ApproverRef =
  | { kind: "user"; id: string }
  | { kind: "role"; role: ApproverRole };

export interface PermissionRule {
  id: string;
  scope: PermissionScope;
  scope_id: string;
  tool_pattern: string;
  state: PermissionState;
  conditions: unknown | null;
  approvers: ApproverRef[];
  allow_self_approval: boolean;
  origin: PermissionOrigin;
  created_by: string | null;
  created_at: string;
  updated_at: string;
}

export interface PermissionRoleDef {
  id: string;
  team_id: string;
  name: string;
  description: string;
  created_at: string;
}

export interface PermissionRolesResponse {
  roles: PermissionRoleDef[];
  count: number;
}

export interface PermissionRulesResponse {
  rules: PermissionRule[];
  count: number;
  generation: number;
}

export interface CascadeLevelPayload {
  rules: PermissionRule[];
  state: PermissionState | null;
}

export interface CascadeResponse {
  request: {
    user_id: string;
    agent_id: string;
    team_ids: string[];
    tool_name: string;
  };
  effective: PermissionState;
  rules_by_scope: {
    team: CascadeLevelPayload;
    agent: CascadeLevelPayload;
    user: CascadeLevelPayload;
    admin_state: PermissionState | null;
  };
}

export interface IdentityRolesResponse {
  identity_id: string;
  roles: Array<{
    role: PermissionRoleDef;
    rules: PermissionRule[];
  }>;
}

export interface CascadeQuery {
  user_id: string;
  agent_id: string;
  tool_name: string;
  team_ids?: string[];
}

export interface RuleFilter {
  scope_type?: PermissionScope;
  scope_id?: string;
  tool_pattern?: string;
  origin?: PermissionOrigin;
  limit?: number;
}

function toQuery(params: Record<string, string | number | undefined>): string {
  const entries = Object.entries(params).filter(
    ([, v]) => v !== undefined && v !== null && v !== "",
  ) as [string, string | number][];
  if (entries.length === 0) return "";
  return (
    "?" +
    entries
      .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`)
      .join("&")
  );
}

export interface CreateRuleBody {
  scope: PermissionScope;
  scope_id: string;
  tool_pattern: string;
  state: PermissionState;
  conditions?: unknown | null;
  approvers?: ApproverRef[];
  allow_self_approval?: boolean;
}

export interface UpdateRuleBody {
  tool_pattern?: string;
  state?: PermissionState;
  conditions?: unknown | null;
  approvers?: ApproverRef[];
  allow_self_approval?: boolean;
}

export interface CreateRoleBody {
  team_id: string;
  name: string;
  description?: string;
}

export const permissionsApi = {
  listRules: (filter: RuleFilter = {}) =>
    gateway.get<PermissionRulesResponse>(
      `/api/permissions/rules${toQuery(filter as Record<string, string | number | undefined>)}`,
    ),

  cascade: (q: CascadeQuery) =>
    gateway.get<CascadeResponse>(
      `/api/permissions/cascade${toQuery({
        user_id: q.user_id,
        agent_id: q.agent_id,
        tool_name: q.tool_name,
        team_ids: q.team_ids?.join(","),
      })}`,
    ),

  listRoles: (teamId: string) =>
    gateway.get<PermissionRolesResponse>(
      `/api/permissions/roles${toQuery({ team_id: teamId })}`,
    ),

  rolesForIdentity: (identityId: string) =>
    gateway.get<IdentityRolesResponse>(
      `/api/permissions/identities/${encodeURIComponent(identityId)}/roles`,
    ),

  createRule: (body: CreateRuleBody) =>
    gateway.post<{ rule: PermissionRule; generation: number }>(
      "/api/permissions/rules",
      body,
    ),

  updateRule: (id: string, body: UpdateRuleBody) =>
    gateway.put<{ rule: PermissionRule; generation: number }>(
      `/api/permissions/rules/${encodeURIComponent(id)}`,
      body,
    ),

  deleteRule: (id: string) =>
    gateway.del<{ deleted: string; generation: number }>(
      `/api/permissions/rules/${encodeURIComponent(id)}`,
    ),

  createRole: (body: CreateRoleBody) =>
    gateway.post<{ role: PermissionRoleDef }>("/api/permissions/roles", body),

  deleteRole: (id: string) =>
    gateway.del<{ deleted: string }>(
      `/api/permissions/roles/${encodeURIComponent(id)}`,
    ),

  rulesForRole: (roleId: string) =>
    gateway.get<{ rules: PermissionRule[]; count: number }>(
      `/api/permissions/roles/${encodeURIComponent(roleId)}/rules`,
    ),

  grantRule: (roleId: string, ruleId: string) =>
    gateway.post<{ role_id: string; rule_id: string; status: string }>(
      `/api/permissions/roles/${encodeURIComponent(roleId)}/rules`,
      { rule_id: ruleId },
    ),

  revokeRule: (roleId: string, ruleId: string) =>
    gateway.del<{ revoked: string }>(
      `/api/permissions/roles/${encodeURIComponent(roleId)}/rules/${encodeURIComponent(ruleId)}`,
    ),

  assignRole: (identityId: string, roleId: string) =>
    gateway.post<{ status: string }>(
      `/api/permissions/identities/${encodeURIComponent(identityId)}/roles`,
      { role_id: roleId },
    ),

  unassignRole: (identityId: string, roleId: string) =>
    gateway.del<{ unassigned: string }>(
      `/api/permissions/identities/${encodeURIComponent(identityId)}/roles/${encodeURIComponent(roleId)}`,
    ),

  listAudit: (filter: AuditFilter = {}) =>
    gateway.get<AuditResponse>(
      `/api/permissions/audit${toQuery(filter as Record<string, string | number | undefined>)}`,
    ),

  getCapabilityMatrix: () =>
    gateway.get<CapabilityMatrixResponse>("/api/capabilities/matrix"),

  updateCapabilityMatrix: (body: UpdateCapabilityMatrixBody) =>
    gateway.put<CapabilityMatrixUpdateResponse>(
      "/api/capabilities/matrix",
      body,
    ),

  getCapabilityAudit: () =>
    gateway.get<CapabilityAuditResponse>("/api/capabilities/audit"),

  getCapabilityCatalog: () =>
    gateway.get<CapabilityCatalogResponse>("/api/capabilities/catalog"),

  getMyCapabilities: () =>
    gateway.get<MyCapabilitiesResponse>("/api/me/capabilities"),

  effective: (q: EffectiveQuery) =>
    gateway.get<EffectiveResponse>(
      `/api/permissions/effective${toQuery({
        user_id: q.user_id,
        agent_id: q.agent_id,
        tool_name: q.tool_name,
        team_ids: q.team_ids?.join(","),
      })}`,
    ),

  listAllRoles: () => gateway.get<RolesResponse>("/api/roles"),

  createCustomRole: (body: CreateCustomRoleBody) =>
    gateway.post<{ status: string; id: string; key: string }>("/api/roles", body),

  updateCustomRole: (id: string, body: UpdateCustomRoleBody) =>
    gateway.put<{ status: string }>(
      `/api/roles/${encodeURIComponent(id)}`,
      body,
    ),

  deleteCustomRole: (id: string) =>
    gateway.del<{ status: string; reassigned: number }>(
      `/api/roles/${encodeURIComponent(id)}`,
    ),
};

export interface AuditFilter {
  operations?: string;
  limit?: number;
  user_id?: string;
  resource?: string;
}

// ── Capability matrix ──────────────────────────────────────────────────────
// The "who can do what" scorecard rendered on the Capabilities tab. Decisions
// reuse the three-way `allow | ask | deny` axis the backend `Decision` enum
// serializes (snake_case). Roles are emitted in matrix order with a stable
// capability ordering, so the UI can pivot the response without hardcoding it.

export type CapabilityDecisionValue = "allow" | "ask" | "deny";

export interface CapabilityDecision {
  capability: string;
  decision: CapabilityDecisionValue;
}

// One matrix column: a role (built-in or custom) with its full scorecard.
export interface RoleRow {
  key: string;
  label: string;
  description: string;
  is_builtin: boolean;
  capabilities: CapabilityDecision[];
}

export interface SystemDefault {
  pattern: string;
  state: string;
  description: string;
}

export interface CapabilityMatrixResponse {
  roles: RoleRow[];
  /** Capability keys that are System-Admin-only and not editable for any role. */
  protected_capabilities: string[];
  system_defaults: SystemDefault[];
}

// A single edited cell. The PUT endpoint accepts only changed cells; `role`
// must be a known role key (built-in or custom).
export interface CapabilityChange {
  role: string;
  capability: string;
  decision: CapabilityDecisionValue;
}

export interface UpdateCapabilityMatrixBody {
  changes: CapabilityChange[];
}

export interface CapabilityMatrixUpdateResponse {
  roles: RoleRow[];
  generation: number;
}

// ── Capability catalog (single source of truth for descriptions) ────────────

export interface CapabilityCatalogEntry {
  key: string;
  label: string;
  description: string;
  category: string;
  tools: string[];
  protected: boolean;
}

export interface CapabilityCatalogResponse {
  capabilities: CapabilityCatalogEntry[];
}

// ── Assignable roles (built-ins + custom) ───────────────────────────────────

export interface RoleInfo {
  key: string;
  label: string;
  description: string;
  is_builtin: boolean;
}

export interface RolesResponse {
  roles: RoleInfo[];
}

export interface CreateCustomRoleBody {
  key: string;
  name: string;
  description?: string;
}

export interface UpdateCustomRoleBody {
  name: string;
  description?: string;
}

// ── Effective-permission lookup (matrix + optional rules overlay) ────────────

export interface EffectiveBranch {
  when: string;
  capability: string | null;
  decision: PermissionState;
}

export interface EffectiveQuery {
  user_id: string;
  agent_id: string;
  tool_name: string;
  team_ids?: string[];
}

export interface EffectiveResponse {
  tool: string;
  user_role: string;
  capability: string | null;
  capability_label: string | null;
  matrix_decision: string;
  rules_enabled: boolean;
  rules_decision: PermissionState | null;
  effective: PermissionState;
  deciding_layer: "capability_matrix" | "rules" | "ungated";
  arg_dependent: boolean;
  branches: EffectiveBranch[] | null;
}

// ── My own capabilities (self-scoped, non-admin) ────────────────────────────

export interface MyCapabilitiesResponse {
  identity_id: string;
  display_name: string;
  role: string;
  capabilities: CapabilityDecision[];
}

// Change history for the matrix. Decisions are nullable because a built-in
// default has no stored override (null == "inherits the default").
export interface CapabilityAuditEntry {
  id: string;
  at: string;
  actor_id: string | null;
  role: string;
  capability: string;
  old_decision: CapabilityDecisionValue | null;
  new_decision: CapabilityDecisionValue | null;
}

export interface CapabilityAuditResponse {
  entries: CapabilityAuditEntry[];
}

export interface AuditEvent {
  timestamp: string;
  operation: string;
  decision: string;
  user_id: string | null;
  owner_or_state: string | null;
  resource: string | null;
}

export interface AuditResponse {
  events: AuditEvent[];
  count: number;
  buffer_size: number;
}
