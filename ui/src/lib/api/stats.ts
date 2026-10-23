import { gateway } from "../api-client";
import type { BillingSplit, TokenHistoryPoint } from "./cost";
import type { CredentialTier } from "./subscriptions";

export interface StatsRange {
  from?: string; // ISO-8601
  to?: string;
}

export interface StatsPage {
  cursor?: string;
  limit?: number;
}

export interface CursorPage<T> {
  entries: T[];
  next_cursor: string | null;
}

export interface WindowDescriptor {
  from: string;
  to: string;
}

export interface TotalsSummary {
  task_count: number;
  cost_usd: number;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  billing: BillingSplit;
}

export interface GroupedRow {
  key: string;
  display_name: string | null;
  cost_usd: number;
}

export interface AgentBillingRow {
  agent_id: string;
  display_name: string | null;
  split: BillingSplit;
}

/** `origin` is null for calls recorded before 2026-09-29, when origins began to be kept. */
export interface OriginRow {
  origin: string | null;
  split: BillingSplit;
}

export interface PayerRow {
  payer: string;
  plan: string;
  credential_tier: CredentialTier | null;
  cost_usd: number;
  calls: number;
}

export interface StatsOverview {
  window: WindowDescriptor;
  totals: TotalsSummary;
  by_identity: GroupedRow[];
  by_agent: GroupedRow[];
  by_model: GroupedRow[];
  by_provider: GroupedRow[];
  by_team: GroupedRow[];
  by_task_type: GroupedRow[];
  by_agent_billing: AgentBillingRow[];
  by_origin: OriginRow[];
  history: TokenHistoryPoint[];
}

export interface CostBreakdown extends StatsOverview {
  by_ticket: GroupedRow[];
}

export interface IdentityRef {
  id: string;
  display_name: string;
  email: string | null;
  role: string | null;
}

export interface UsageSummaryRow {
  group_key: string;
  total_tasks: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost: number;
  avg_rounds: number;
  avg_duration_ms: number;
  success_rate: number;
  total_cache_created_tokens: number;
  total_cache_read_tokens: number;
}

export interface ModelComparison {
  model: string;
  total_tasks: number;
  avg_input_tokens: number;
  avg_output_tokens: number;
  avg_cost: number;
  avg_rounds: number;
  avg_latency_ms: number;
  success_rate: number;
  total_cost: number;
  avg_cache_read_tokens: number;
  total_cache_read_tokens: number;
}

export interface ToolBreakdown {
  tool_name: string;
  invocations: number;
  total_duration_ms: number;
  avg_duration_ms: number;
  total_input_bytes: number;
  total_output_bytes: number;
  successes: number;
  success_rate_pct: number;
  estimated_token_cost: number;
}

export interface IdentityStats {
  identity: IdentityRef;
  window: WindowDescriptor;
  totals: TotalsSummary;
  by_model: UsageSummaryRow[];
  by_task_type: UsageSummaryRow[];
  by_agent: UsageSummaryRow[];
  model_comparison: ModelComparison[];
  tool_breakdown: ToolBreakdown[];
  history: TokenHistoryPoint[];
}

export interface AgentStats {
  agent_id: string;
  window: WindowDescriptor;
  totals: TotalsSummary;
  by_model: UsageSummaryRow[];
  by_task_type: UsageSummaryRow[];
  by_identity: GroupedRow[];
  by_origin: OriginRow[];
  by_payer: PayerRow[];
  model_comparison: ModelComparison[];
  tool_breakdown: ToolBreakdown[];
  history: TokenHistoryPoint[];
  round_outliers: TaskHistoryEntry[];
}

export interface TaskHistoryEntry {
  id: number;
  agent_id: string;
  model: string;
  task_type: string | null;
  task_id: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  cost_usd: number;
  rounds: number;
  duration_ms: number;
  success: boolean;
  completed_at: string;
  model_switches: string | null;
  ticket_id: string | null;
  identity_id: string | null;
  round_count: number;
  tool_call_count: number;
}

export interface RoundTelemetry {
  round_number: number;
  input_tokens: number;
  output_tokens: number;
  latency_ms: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  prompt_size_bytes: number;
  response_size_bytes: number;
  tool_count: number;
  stop_reason: string;
}

export interface ToolTelemetry {
  round_number: number;
  tool_name: string;
  success: boolean;
  duration_ms: number;
  policy_decision: string;
  input_size_bytes: number;
  output_size_bytes: number;
  denial_reason: string | null;
}

export interface CostRecordView {
  timestamp: string;
  agent_id: string;
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
  identity_id: string | null;
}

export interface TaskTelemetry {
  agent_id: string;
  model: string;
  provider: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  cost_usd: number;
  rounds: number;
  duration_ms: number;
  tool_invocation_count: number;
  success: boolean;
  error_type: string | null;
  retry_count: number;
  task_type: string | null;
  task_id: string | null;
  started_at: string;
  completed_at: string;
  team_id: string | null;
  session_id: string | null;
  model_switches: string | null;
  ticket_id: string | null;
  identity_id: string | null;
}

export interface TaskTrace {
  task: TaskTelemetry;
  rounds: RoundTelemetry[];
  tools: ToolTelemetry[];
  cost_records: CostRecordView[];
  identity: IdentityRef | null;
}

export interface EfficiencyTrendPoint {
  bucket: string;
  total_tasks: number;
  avg_input_tokens: number;
  avg_output_tokens: number;
  avg_cost: number;
  avg_rounds: number;
  avg_latency_ms: number;
  success_rate: number;
}

export interface EfficiencyResponse {
  trends: EfficiencyTrendPoint[];
  outliers: TaskHistoryEntry[];
}

function rangeQs(range: StatsRange): string {
  const params = new URLSearchParams();
  if (range.from) params.set("from", range.from);
  if (range.to) params.set("to", range.to);
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

function pageQs(range: StatsRange, page: StatsPage): string {
  const params = new URLSearchParams();
  if (range.from) params.set("from", range.from);
  if (range.to) params.set("to", range.to);
  if (page.cursor) params.set("cursor", page.cursor);
  if (page.limit !== undefined) params.set("limit", String(page.limit));
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

export const statsApi = {
  overview: (range: StatsRange) =>
    gateway.get<StatsOverview>(`/api/stats/overview${rangeQs(range)}`),
  costBreakdown: (range: StatsRange) =>
    gateway.get<CostBreakdown>(`/api/stats/cost/breakdown${rangeQs(range)}`),
  identities: (range: StatsRange, page: StatsPage = {}) =>
    gateway.get<CursorPage<GroupedRow>>(
      `/api/stats/identities${pageQs(range, page)}`,
    ),
  identity: (id: string, range: StatsRange) =>
    gateway.get<IdentityStats>(
      `/api/stats/identities/${encodeURIComponent(id)}${rangeQs(range)}`,
    ),
  identityTasks: (id: string, range: StatsRange, page: StatsPage = {}) =>
    gateway.get<CursorPage<TaskHistoryEntry>>(
      `/api/stats/identities/${encodeURIComponent(id)}/tasks${pageQs(range, page)}`,
    ),
  agents: (range: StatsRange, page: StatsPage = {}) =>
    gateway.get<CursorPage<GroupedRow>>(
      `/api/stats/agents${pageQs(range, page)}`,
    ),
  agent: (id: string, range: StatsRange) =>
    gateway.get<AgentStats>(
      `/api/stats/agents/${encodeURIComponent(id)}${rangeQs(range)}`,
    ),
  agentTasks: (id: string, range: StatsRange, page: StatsPage = {}) =>
    gateway.get<CursorPage<TaskHistoryEntry>>(
      `/api/stats/agents/${encodeURIComponent(id)}/tasks${pageQs(range, page)}`,
    ),
  taskTrace: (taskId: string) =>
    gateway.get<TaskTrace>(`/api/stats/tasks/${encodeURIComponent(taskId)}`),
  models: (range: StatsRange) =>
    gateway.get<GroupedRow[]>(`/api/stats/models${rangeQs(range)}`),
  tools: (range: StatsRange, limit?: number) => {
    const qs = pageQs(range, { limit });
    return gateway.get<ToolBreakdown[]>(`/api/stats/tools${qs}`);
  },
  efficiency: (range: StatsRange) =>
    gateway.get<EfficiencyResponse>(`/api/stats/efficiency${rangeQs(range)}`),
};
