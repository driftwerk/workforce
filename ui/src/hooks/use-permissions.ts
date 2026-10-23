import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  permissionsApi,
  type CascadeQuery,
  type CreateCustomRoleBody,
  type CreateRoleBody,
  type CreateRuleBody,
  type EffectiveQuery,
  type RuleFilter,
  type UpdateCapabilityMatrixBody,
  type UpdateCustomRoleBody,
  type UpdateRuleBody,
} from "@/lib/api/permissions";

export function usePermissionRules(filter: RuleFilter = {}) {
  return useQuery({
    queryKey: ["permission-rules", filter],
    queryFn: () => permissionsApi.listRules(filter),
    staleTime: 30_000,
  });
}

export function usePermissionCascade(query: CascadeQuery | null) {
  return useQuery({
    queryKey: ["permission-cascade", query],
    queryFn: () => {
      if (!query) throw new Error("cascade query not provided");
      return permissionsApi.cascade(query);
    },
    enabled: !!query,
    staleTime: 5_000,
  });
}

export function usePermissionRoles(teamId: string | null) {
  return useQuery({
    queryKey: ["permission-roles", teamId],
    queryFn: () => permissionsApi.listRoles(teamId as string),
    enabled: !!teamId,
    staleTime: 30_000,
  });
}

export function useIdentityPermissionRoles(identityId: string | null) {
  return useQuery({
    queryKey: ["permission-identity-roles", identityId],
    queryFn: () => permissionsApi.rolesForIdentity(identityId as string),
    enabled: !!identityId,
    staleTime: 30_000,
  });
}

function invalidateRulesAndCascade(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["permission-rules"] });
  qc.invalidateQueries({ queryKey: ["permission-cascade"] });
}

export function useCreatePermissionRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateRuleBody) => permissionsApi.createRule(body),
    onSuccess: () => invalidateRulesAndCascade(qc),
  });
}

export function useUpdatePermissionRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, body }: { id: string; body: UpdateRuleBody }) =>
      permissionsApi.updateRule(id, body),
    onSuccess: () => invalidateRulesAndCascade(qc),
  });
}

export function useDeletePermissionRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => permissionsApi.deleteRule(id),
    onSuccess: () => invalidateRulesAndCascade(qc),
  });
}

export function useCreatePermissionRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateRoleBody) => permissionsApi.createRole(body),
    onSuccess: (_, vars) => {
      qc.invalidateQueries({ queryKey: ["permission-roles", vars.team_id] });
    },
  });
}

export function useDeletePermissionRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => permissionsApi.deleteRole(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["permission-roles"] });
      qc.invalidateQueries({ queryKey: ["permission-identity-roles"] });
    },
  });
}

export function useAssignPermissionRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      identityId,
      roleId,
    }: {
      identityId: string;
      roleId: string;
    }) => permissionsApi.assignRole(identityId, roleId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["permission-identity-roles"] });
    },
  });
}

export function useUnassignPermissionRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      identityId,
      roleId,
    }: {
      identityId: string;
      roleId: string;
    }) => permissionsApi.unassignRole(identityId, roleId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["permission-identity-roles"] });
    },
  });
}

export function useRulesForRole(roleId: string | null) {
  return useQuery({
    queryKey: ["permission-role-rules", roleId],
    queryFn: () => permissionsApi.rulesForRole(roleId as string),
    enabled: !!roleId,
    staleTime: 30_000,
  });
}

function invalidateRoleRules(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["permission-role-rules"] });
  qc.invalidateQueries({ queryKey: ["permission-identity-roles"] });
}

export function useGrantPermissionRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ roleId, ruleId }: { roleId: string; ruleId: string }) =>
      permissionsApi.grantRule(roleId, ruleId),
    onSuccess: () => invalidateRoleRules(qc),
  });
}

export function useRevokePermissionRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ roleId, ruleId }: { roleId: string; ruleId: string }) =>
      permissionsApi.revokeRule(roleId, ruleId),
    onSuccess: () => invalidateRoleRules(qc),
  });
}

export function usePermissionAudit(
  filter: import("@/lib/api/permissions").AuditFilter = {},
) {
  return useQuery({
    queryKey: ["permission-audit", filter],
    queryFn: () => permissionsApi.listAudit(filter),
    staleTime: 5_000,
    refetchInterval: 10_000,
  });
}

export function useCapabilityMatrix() {
  return useQuery({
    queryKey: ["capability-matrix"],
    queryFn: () => permissionsApi.getCapabilityMatrix(),
    staleTime: 30_000,
  });
}

export function useUpdateCapabilityMatrix() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: UpdateCapabilityMatrixBody) =>
      permissionsApi.updateCapabilityMatrix(body),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["capability-matrix"] });
      qc.invalidateQueries({ queryKey: ["capability-audit"] });
    },
  });
}

export function useCapabilityAudit() {
  return useQuery({
    queryKey: ["capability-audit"],
    queryFn: () => permissionsApi.getCapabilityAudit(),
    staleTime: 5_000,
  });
}

export function useCapabilityCatalog() {
  return useQuery({
    queryKey: ["capability-catalog"],
    queryFn: () => permissionsApi.getCapabilityCatalog(),
    staleTime: 5 * 60_000,
  });
}

export function useMyCapabilities() {
  return useQuery({
    queryKey: ["my-capabilities"],
    queryFn: () => permissionsApi.getMyCapabilities(),
    staleTime: 30_000,
    // Token-only admin logins carry no identity and get a 401 here; don't retry
    // an auth failure that won't recover.
    retry: false,
  });
}

/** Whether the caller holds `capability` — mirrors the backend `enforce`, where
 *  a non-`deny` decision (`allow`/`ask`) permits the action. `false` while the
 *  self-capability query is loading or errors (e.g. token-only admin logins). */
export function useHasCapability(capability: string): boolean {
  const { data } = useMyCapabilities();
  return (
    data?.capabilities.some(
      (c) => c.capability === capability && c.decision !== "deny",
    ) ?? false
  );
}

export function useEffectivePermission(query: EffectiveQuery | null) {
  return useQuery({
    queryKey: ["permission-effective", query],
    queryFn: () => {
      if (!query) throw new Error("effective query not provided");
      return permissionsApi.effective(query);
    },
    enabled: !!query,
    staleTime: 5_000,
  });
}

/** Built-in + custom roles — the assignable role catalogue. */
export function useAllRoles() {
  return useQuery({
    queryKey: ["roles-all"],
    queryFn: () => permissionsApi.listAllRoles(),
    staleTime: 30_000,
  });
}

function invalidateRolesAndMatrix(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["roles-all"] });
  qc.invalidateQueries({ queryKey: ["capability-matrix"] });
}

export function useCreateCustomRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateCustomRoleBody) => permissionsApi.createCustomRole(body),
    onSuccess: () => invalidateRolesAndMatrix(qc),
  });
}

export function useUpdateCustomRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, body }: { id: string; body: UpdateCustomRoleBody }) =>
      permissionsApi.updateCustomRole(id, body),
    onSuccess: () => invalidateRolesAndMatrix(qc),
  });
}

export function useDeleteCustomRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => permissionsApi.deleteCustomRole(id),
    onSuccess: () => {
      invalidateRolesAndMatrix(qc);
      qc.invalidateQueries({ queryKey: ["identities"] });
    },
  });
}
