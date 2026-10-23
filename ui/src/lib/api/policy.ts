import { gateway } from "../api-client";

export interface PolicyRule {
  name: string;
  scope: string;
  tool: string;
  action: string;
  priority?: number;
  constitutional?: boolean;
  conditions?: Record<string, unknown>;
}

export interface PolicyProposal {
  id: string;
  agent_id?: string;
  proposed_rule: PolicyRule;
  justification: string;
  status: "pending" | "approved" | "rejected";
  reviewed_by?: string;
  review_note?: string;
  created_at: string;
  reviewed_at?: string;
}

export const policyApi = {
  listRules: () => gateway.get<{ rules: PolicyRule[] }>("/api/policy/rules"),
  createRule: (rule: PolicyRule) =>
    gateway.post<{ status: string }>("/api/policy/rules", rule),
  updateRule: (name: string, rule: Partial<PolicyRule>) =>
    gateway.put<{ status: string }>(`/api/policy/rules/${name}`, rule),
  deleteRule: (name: string) =>
    gateway.del<{ status: string }>(`/api/policy/rules/${name}`),

  listProposals: (status?: string) =>
    gateway.get<{ proposals: PolicyProposal[] }>(
      `/api/policy/proposals${status ? `?status=${status}` : ""}`,
    ),
  approveProposal: (id: string, note?: string) =>
    gateway.post<{ status: string }>(`/api/policy/proposals/${id}/approve`, {
      note,
    }),
  rejectProposal: (id: string, note?: string) =>
    gateway.post<{ status: string }>(`/api/policy/proposals/${id}/reject`, {
      note,
    }),
};
