import { admin } from "../api-client";
import type { CredentialTier } from "./subscriptions";

export type PrincipalKind = "user" | "system" | "unknown";

export interface PrincipalRef {
  id: string;
  display_name: string;
  kind: PrincipalKind;
}

export type Billing = "metered" | "subscription";

export interface BillingSplit {
  metered_cost_usd: number;
  subscription_cost_usd: number;
  subscription_calls: number;
}

export interface AgentRef {
  id: string;
  display_name: string;
}

export interface CostSummary {
  total_cost_usd: number;
  total_input_tokens: number;
  total_output_tokens: number;
  by_agent: Record<string, number>;
  by_model: Record<string, number>;
  record_count: number;
}

export interface TokenHistoryPoint {
  bucket_start: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
}

export interface TokenHistory {
  days: number;
  points: TokenHistoryPoint[];
}

/** One row from the per-call cost ledger (cost_records). One row per billable
 *  LLM call across agent runs, vision, transcription, and workflow steps. */
export interface CostDetailRecord {
  id: number;
  timestamp: string;
  agent_id: string;
  agent_display_name: string;
  model: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  cost_usd: number;
  team_id: string | null;
  task_id: string | null;
  task_type: string | null;
  ticket_id: string | null;
  user_id: string | null;
  user_display_name: string | null;
  user_kind: PrincipalKind | null;
  description: string | null;
  origin: string | null;
  billing: Billing;
  payer: string | null;
  /** Whose plan paid, as a person reads it; null on a metered row. */
  plan: string | null;
  credential_tier: CredentialTier | null;
}

export interface CostDetailResponse {
  records: CostDetailRecord[];
  total: number;
  /** Distinct user identities in the time + agent range, hydrated with
   *  display names. The dropdown intentionally does not narrow itself
   *  when a user filter is active, so the full set stays available. */
  distinct_users: PrincipalRef[];
  /** Distinct agents in the time range (no agent or user narrowing). */
  distinct_agents: AgentRef[];
  /** Both billing figures over the same filters, ignoring `billing`. */
  billing_totals: BillingSplit;
  limit: number;
  offset: number;
}

export interface CostDetailParams {
  start?: string;
  end?: string;
  agent_id?: string;
  user_id?: string;
  billing?: Billing;
  limit?: number;
  offset?: number;
}

/** One bucket of the monthly cost breakdown chart. */
export interface MonthlyPoint {
  month: string; // "YYYY-MM"
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  cost_usd: number;
}

export interface CostMonthlyResponse {
  months: number;
  points: MonthlyPoint[];
}

/** One row from the scheduled-job execution log. Backs the scheduler UI's
 *  history tab. `cost_usd` is summed from `cost_records.task_id`, so it's
 *  zero for pure tool/shell/monitor jobs (which never call an LLM). */
export interface ScheduledJobRecord {
  id: number;
  agent_id: string;
  agent_display_name: string;
  task_type: string | null;
  task_id: string | null;
  description: string | null;
  success: boolean;
  error_type: string | null;
  duration_ms: number;
  started_at: string;
  completed_at: string;
  user_id: string | null;
  user_display_name: string | null;
  user_kind: PrincipalKind | null;
  cost_usd: number;
}

export interface ScheduledJobHistoryResponse {
  records: ScheduledJobRecord[];
  total: number;
  limit: number;
  offset: number;
}

export interface ScheduledJobHistoryParams {
  start?: string;
  end?: string;
  agent_id?: string;
  limit?: number;
  offset?: number;
}

function buildSearch(params: object): string {
  const search = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") search.set(k, String(v));
  }
  const qs = search.toString();
  return qs ? `?${qs}` : "";
}

export const costApi = {
  summary: () => admin.get<CostSummary>("/api/cost"),
  history: (days: number) =>
    admin.get<TokenHistory>(`/api/cost/history?days=${days}`),
  monthly: (months = 12) =>
    admin.get<CostMonthlyResponse>(`/api/cost/monthly?months=${months}`),
  detail: (params: CostDetailParams = {}) =>
    admin.get<CostDetailResponse>(`/api/cost/detail${buildSearch(params)}`),
  scheduledJobHistory: (params: ScheduledJobHistoryParams = {}) =>
    admin.get<ScheduledJobHistoryResponse>(
      `/api/scheduler/jobs/history${buildSearch(params)}`,
    ),
};
