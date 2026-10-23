import { gateway } from "../api-client";

export interface ScopeStatus {
  key: string;
  spent_usd: number;
  limit_usd: number;
  unlimited: boolean;
  overridden: boolean;
  override_expires_at: string | null;
}

export interface BudgetOverride {
  scope: "hub" | "agent" | "identity";
  key: string;
  daily_usd: number;
  expires_at: string | null;
  note: string | null;
  created_by: string;
  updated_at: string;
}

export interface OverrideAuditEntry {
  action: string;
  scope: "hub" | "agent" | "identity";
  key: string;
  daily_usd: number | null;
  expires_at: string | null;
  actor: string;
  at: string;
}

export interface BudgetStatusResponse {
  enabled: boolean;
  day: string;
  hub: ScopeStatus;
  agents: ScopeStatus[];
  identities: ScopeStatus[];
  overrides: BudgetOverride[];
  identity_names: Record<string, string>;
  audit: OverrideAuditEntry[];
}

export interface PutOverrideBody {
  scope: "hub" | "agent" | "identity";
  key: string;
  daily_usd: number;
  /** "today", a number of hours, or omitted for no expiry. */
  expires?: string | number;
  note?: string;
}

export const budgetApi = {
  status: () => gateway.get<BudgetStatusResponse>("/api/budget/status"),
  putOverride: (body: PutOverrideBody) =>
    gateway.put<{ saved: boolean }>("/api/budget/overrides", body),
  clearOverride: (scope: string, key: string) =>
    gateway.post<{ removed: boolean }>("/api/budget/overrides/clear", { scope, key }),
};
