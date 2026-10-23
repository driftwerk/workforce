import { admin, gateway } from "../api-client";
import type { DefRevision } from "./revisions";

// --- Summary type for list endpoint (matches backend WorkflowSummary) ---

/**
 * Where one HTTP trigger answers. `url` is the address the hub minted for it,
 * which is unaffected by who owns the workflow; `legacy_url` is the older
 * owner-derived address, present only while it still answers. Either may be
 * absent, so neither is ever shown as working when it is not.
 */
export interface TriggerUrl {
  trigger_id: string;
  url?: string;
  legacy_url?: string;
}

export interface WorkflowDef {
  id: string;
  name: string;
  description: string;
  steps: number;
  owner?: string | null;
  visibility?: string;
  /** Identity automated runs execute as when it is not the owner. */
  run_as?: string | null;
  /** Why automated runs are paused, for a definition with triggers. */
  automated_runs_blocked?: string | null;
  /** Absent for a workflow with no HTTP trigger. */
  trigger_urls?: TriggerUrl[];
}

/** Who a member-owned workflow's automated runs execute as, chosen at save. */
export type RunAs = "owner" | "me";

// --- Full workflow definition types (matches Rust WorkflowDefinition from TOML) ---

export interface ContextField {
  required: boolean;
  description: string;
}

export interface OutputField {
  name: string;
  description: string;
  type?: "string" | "number" | "integer" | "boolean" | "list" | "object";
  required?: boolean;
  validate?: string;
}

export interface Branch {
  when: string;
  goto: string;
}

export type DecisionStyle = "primary" | "danger";

/** One button of a decision step (`[[step.decision.buttons]]`). */
export interface DecisionButton {
  label: string;
  outcome: string;
  style?: DecisionStyle;
  done?: string;
  confirm?: string;
  goto?: string;
  end?: "complete" | "cancel";
  workflow?: string | { id: string; wait?: boolean; payload?: Record<string, string> };
  set?: Record<string, string>;
}

/** A step's `[step.decision]`: named people choose what happens next. */
export interface DecisionSpec {
  title?: string;
  context?: string;
  channel?: string;
  deciders: string | string[];
  allow_self_decision?: boolean;
  expires_after?: string;
  on_expiry?: string;
  buttons: DecisionButton[];
}

/**
 * A workflow step. The visual editor edits the typed fields below and reads
 * `tool` and `report` to label a step (`report_render` is typed alongside
 * them); everything else an author wrote (`model`, `phases`, `skills`, …) is
 * carried through the index signature so a visual edit + save cannot strip
 * it — under `delivery = "reports"` a step's `report` is the workflow's only
 * channel voice, so losing it silences the run. `task` is empty on a
 * report-only step.
 */
export interface StepDefinition {
  id: string;
  name: string;
  task: string;
  branches: Branch[];
  goto?: string;
  pause?: string;
  exit?: boolean;
  handoff?: string;
  skill?: string;
  agent?: string;
  outputs: OutputField[];
  tool?: string;
  decide?: { question: string; kind?: "choice" | "noul" | "score" };
  workflow?: string | { id: string; wait?: boolean };
  report?: string;
  report_render?: "agent" | "template";
  decision?: DecisionSpec;
  [key: string]: unknown;
}

/**
 * A workflow trigger. The visual editor passes triggers through verbatim — it
 * edits steps, not triggers — so every field is preserved across kinds
 * (webhook / schedule / http), plus any future field via the index signature.
 */
export interface TriggerDefinition {
  type?: string;
  // webhook
  source?: string;
  event_type?: string;
  match?: Record<string, string>;
  secret?: string;
  // schedule
  cron?: string;
  tz?: string;
  run_as?: string;
  payload?: Record<string, unknown>;
  // http
  id?: string;
  auth?: Record<string, unknown>;
  response?: string;
  [key: string]: unknown;
}

export interface WorkflowMeta {
  id: string;
  name: string;
  description: string;
  version: string;
  agent?: string;
  context: Record<string, ContextField>;
  delivery?: "full" | "reports";
  /**
   * Whether people other than a run's starter may steer it (approve, reject,
   * resume, skip, stop, …) when the workflow reaches them. Absent means yes.
   */
  steerable_by_others?: boolean;
  /** Workflow-level fields the editor does not type, carried through a save. */
  [key: string]: unknown;
}

/**
 * A typed workflow variable. The visual editor passes variables through
 * verbatim (it edits steps, not variables), so every field — including a
 * secret's vault-marker value and allowed_hosts — survives a round-trip.
 */
export interface VariableDef {
  id: string;
  type?: string;
  value: string;
  allowed_hosts?: string[];
  [key: string]: unknown;
}

export interface WorkflowDefinition {
  workflow: WorkflowMeta;
  step: StepDefinition[];
  trigger?: TriggerDefinition[];
  variables?: VariableDef[];
}

// --- Workflow instance ---

export interface WorkflowInstance {
  instance_id: string;
  workflow_id: string;
  status: string;
  current_step: string | null;
  /** Why a failed instance stopped; absent on every other status. */
  failure_reason?: string | null;
  created_at: string;
  updated_at: string;
  /** The decision the run waits on, while it waits on one. */
  decision?: DecisionView | null;
  /** Set on listed instances: the run waits on a decision only its deciders can press. */
  awaiting_decision?: boolean;
}

/** A choice of an open decision. */
export interface DecisionChoice {
  label: string;
  outcome: string;
  style: DecisionStyle | null;
  confirm: string | null;
}

/** The decision a paused run waits on, as the caller sees it. */
export interface DecisionView {
  gate: string;
  step_id: string;
  title: string;
  context: string | null;
  choices: DecisionChoice[];
  waiting_on: string[];
  expires_at: string | null;
  /** The label of the choice expiry presses. */
  on_expiry: string | null;
  may_decide: boolean;
}

/** A recorded step as the instance endpoints return it. */
export interface InstanceStep {
  step_id: string;
  /** completed, or failed for the step a failed instance stopped on. */
  status: string;
  outcome: string | null;
  started_at: string;
  completed_at: string;
  duration_ms: number;
  rounds: number;
  cost_usd: number;
  engine: string | null;
  model: string | null;
  /** What the decision model answered when asked before the turn, when the
   *  answer reached its floor: `settled` when no turn ran. */
  decided_first?: { settled: boolean; answer: string } | null;
  input_tokens: number | null;
  output_tokens: number | null;
  budget_capped: boolean;
  outputs: Record<string, unknown> | null;
  report: string | null;
  raw_response: string;
  error: string | null;
}

/** A timeline entry: a recorded step named from the definition, or the step
 *  the instance is on now, which has no record yet. */
export interface TimelineStep {
  step_id: string;
  name: string;
  status: string;
  record: InstanceStep | null;
}

export interface InstanceCost {
  total_cost: number;
  total_tokens: number;
  total_rounds: number;
  total_duration_ms: number;
  steps: Array<{ step_id: string; cost: number; tokens: number; rounds: number; duration_ms: number }>;
}

export interface InstanceVariable {
  key: string;
  value: unknown;
}

// --- API client ---

/** `?owner=<id>` query segment, or empty for a system/global definition. */
function ownerQuery(owner?: string | null): string {
  return owner ? `?owner=${encodeURIComponent(owner)}` : "";
}

/**
 * Split a workflow instance's `workflow_id` (the internal `{owner}/{id}`
 * addressing key, or a bare id for system definitions) into the bare id and
 * owner the source/run endpoints expect.
 */
export function splitWorkflowKey(key: string): { id: string; owner: string | null } {
  const i = key.indexOf("/");
  return i < 0 ? { id: key, owner: null } : { owner: key.slice(0, i), id: key.slice(i + 1) };
}

export const workflowsApi = {
  list: () => gateway.get<{ workflows: WorkflowDef[] }>("/api/workflows"),
  // `id` is the bare workflow id; `owner` (a member's identity, omitted for
  // system/global defs) disambiguates which owner's definition to address.
  getSource: (id: string, owner?: string | null) =>
    gateway.get<string>(`/api/workflows/${id}/source${ownerQuery(owner)}`),
  save: (
    id: string,
    toml: string,
    from?: string,
    visibility?: string,
    owner?: string | null,
    runAs?: RunAs,
  ) => {
    const params = new URLSearchParams();
    if (from && from !== id) params.set("from", from);
    if (visibility) params.set("visibility", visibility);
    if (owner) params.set("owner", owner);
    if (runAs) params.set("run_as", runAs);
    const qs = params.toString();
    return gateway.putText<{ saved: string }>(
      `/api/workflows/${id}${qs ? `?${qs}` : ""}`,
      toml,
    );
  },
  del: (id: string, owner?: string | null) =>
    gateway.del<{ deleted: string }>(`/api/workflows/${id}${ownerQuery(owner)}`),
  // Revision history.
  listRevisions: (id: string, owner?: string | null) =>
    gateway.get<{ revisions: DefRevision[] }>(`/api/workflows/${id}/revisions${ownerQuery(owner)}`),
  getRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.get<DefRevision>(`/api/workflows/${id}/revisions/${rev}${ownerQuery(owner)}`),
  restoreRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/workflows/${id}/revisions/${rev}/restore${ownerQuery(owner)}`,
    ),
  run: (id: string, payload?: unknown, channelId?: string, threadId?: string, owner?: string | null) =>
    gateway.post<{ instance_id: string }>(`/api/workflows/${id}/run${ownerQuery(owner)}`, { payload, channel_id: channelId, thread_id: threadId }),
  listInstances: (status?: string, workflowId?: string) => {
    const params = new URLSearchParams();
    if (status) params.set("status", status);
    if (workflowId) params.set("workflow_id", workflowId);
    const qs = params.toString();
    return admin.get<{ instances: WorkflowInstance[]; count: number }>(`/api/workflow-instances${qs ? `?${qs}` : ""}`);
  },
  getInstance: (id: string) => admin.get<WorkflowInstance>(`/api/workflow-instances/${id}`),
  resume: (id: string, outcome?: string, gate?: string) =>
    admin.post(`/api/workflow-instances/${id}/resume`, { outcome, gate }),
  abort: (id: string) => admin.post(`/api/workflow-instances/${id}/abort`),
  reject: (id: string, reason?: string, feedback?: string) =>
    admin.post(`/api/workflow-instances/${id}/reject`, { reason, feedback }),
  getSteps: (id: string) => admin.get<{ steps: InstanceStep[] }>(`/api/workflow-instances/${id}/steps`),
  getCost: (id: string) => admin.get<InstanceCost>(`/api/workflow-instances/${id}/cost`),
  getVariables: (id: string) => admin.get<{ variables: InstanceVariable[] }>(`/api/workflow-instances/${id}/variables`),
  retryStep: (instanceId: string, stepId: string) =>
    admin.post(`/api/workflow-instances/${instanceId}/retry`, { step_id: stepId }),
  skipStep: (instanceId: string, stepId: string) =>
    admin.post(`/api/workflow-instances/${instanceId}/skip`, { step_id: stepId }),
  gotoStep: (instanceId: string, stepId: string) =>
    admin.post(`/api/workflow-instances/${instanceId}/goto`, { step_id: stepId }),
  pause: (id: string) => admin.post(`/api/workflow-instances/${id}/pause`),
  export: () => gateway.get<Record<string, string>>("/api/workflows/export"),
};
