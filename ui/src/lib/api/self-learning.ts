import { gateway } from "../api-client";

export type ItemStatus =
  | "provisional"
  | "approved"
  | "rejected"
  | "stale"
  | "superseded";

export interface PlanSource {
  skill: string;
  instance: string | null;
  scope: string | null;
}

export type CurationMode = "items" | "deep_expertise" | "experiential_expertise";

export interface PlanEstimate {
  per_run_usd: number;
  runs_per_day: number;
  per_day_usd: number;
  per_week_usd: number;
  basis: string;
  warn: boolean;
}

export interface Plan {
  id: string;
  name: string;
  enabled: boolean;
  schedule: string | null;
  interval_secs: number | null;
  estimate: PlanEstimate | null;
  run_as_identity: string | null;
  priority: number;
  max_items_per_run: number | null;
  freshness_days: number | null;
  model: string | null;
  mode: CurationMode;
  source: string | null;
  sources: PlanSource[];
  body: string;
  last_run: string | null;
  item_count: number;
  provisional_count: number;
  approved_count: number;
  node_count: number;
}

export interface PlanUpsertSource {
  skill: string;
  instance?: string;
  scope?: string;
}

export interface PlanUpsert {
  id: string;
  name: string;
  enabled: boolean;
  schedule?: string;
  interval_secs?: number;
  run_as_identity?: string;
  priority?: number;
  max_items_per_run?: number;
  freshness_days?: number;
  model?: string;
  mode?: CurationMode;
  source?: string;
  sources: PlanUpsertSource[];
  body: string;
}

export interface ItemSource {
  kind: string;
  skill: string | null;
  reference: string;
  excerpt: string | null;
}

export interface Item {
  id: string;
  plan_id: string;
  topic: string;
  claim: string;
  status: ItemStatus;
  confidence: number;
  high_stakes: boolean;
  source: ItemSource;
  created_at: string;
  created_by: string;
  verified_at: string;
  expires_at: string | null;
  reviewed_at: string | null;
  reviewed_by: string | null;
  review_note: string | null;
  superseded_by: string | null;
}

export interface PlanListResponse {
  plans: Plan[];
}

export interface ItemListResponse {
  items: Item[];
}

export interface DeletePlanOutcome {
  agent_id: string;
  plan: string;
}

export interface DeleteItemOutcome {
  agent_id: string;
  item: string;
}

export interface RunPlanOutcome {
  plan: string;
  queued: boolean;
}

export interface ItemFilters {
  status?: ItemStatus;
  plan?: string;
  topic?: string;
}

export type NodeStatus =
  | "seeded"
  | "modelled"
  | "verified"
  | "approved"
  | "rejected"
  | "stale"
  | "superseded";

export interface NodeSource {
  kind: string;
  skill: string | null;
  reference: string;
  excerpt: string | null;
}

export interface NodeCoverage {
  reference: string;
  covered: boolean;
}

export interface DeepNode {
  id: string;
  agent_id: string;
  plan_id: string;
  parent_id: string | null;
  depth: number;
  title: string;
  body: string;
  status: NodeStatus;
  confidence: number;
  sources: NodeSource[];
  coverage: NodeCoverage[];
  created_at: string;
  created_by: string;
  verified_at: string;
  expires_at: string | null;
  reviewed_at: string | null;
  reviewed_by: string | null;
  review_note: string | null;
  superseded_by: string | null;
}

export interface NodeListResponse {
  nodes: DeepNode[];
}

export interface NodeDetailResponse {
  node: DeepNode;
  children: DeepNode[];
}

export interface DeleteNodeOutcome {
  agent_id: string;
  node: string;
  deleted: number;
}

export interface NodeFilters {
  status?: NodeStatus;
  plan?: string;
  /** Absent → roots; a node id → that node's children. */
  parent?: string;
}

function plansPath(agentId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/self-learning/plans`;
}

function planPath(agentId: string, planId: string): string {
  return `${plansPath(agentId)}/${encodeURIComponent(planId)}`;
}

function itemsPath(agentId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/self-learning/items`;
}

function itemPath(agentId: string, itemId: string): string {
  return `${itemsPath(agentId)}/${encodeURIComponent(itemId)}`;
}

function itemsQuery(filters: ItemFilters): string {
  const params = new URLSearchParams();
  if (filters.status) params.set("status", filters.status);
  if (filters.plan) params.set("plan", filters.plan);
  if (filters.topic) params.set("topic", filters.topic);
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

function nodesPath(agentId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/deep-expertise/nodes`;
}

function nodePath(agentId: string, nodeId: string): string {
  return `${nodesPath(agentId)}/${encodeURIComponent(nodeId)}`;
}

function nodesQuery(filters: NodeFilters): string {
  const params = new URLSearchParams();
  if (filters.status) params.set("status", filters.status);
  if (filters.plan) params.set("plan", filters.plan);
  if (filters.parent) params.set("parent", filters.parent);
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

export const selfLearningApi = {
  listPlans: (agentId: string) =>
    gateway.get<PlanListResponse>(plansPath(agentId)),

  getPlan: (agentId: string, planId: string) =>
    gateway.get<Plan>(planPath(agentId, planId)),

  upsertPlan: (agentId: string, plan: PlanUpsert) =>
    gateway.post<Plan>(plansPath(agentId), plan),

  deletePlan: (agentId: string, planId: string) =>
    gateway.del<DeletePlanOutcome>(planPath(agentId, planId)),

  runPlan: (agentId: string, planId: string) =>
    gateway.post<RunPlanOutcome>(`${planPath(agentId, planId)}/run`),

  listItems: (agentId: string, filters: ItemFilters = {}) =>
    gateway.get<ItemListResponse>(`${itemsPath(agentId)}${itemsQuery(filters)}`),

  approveItem: (agentId: string, itemId: string) =>
    gateway.post<Item>(`${itemPath(agentId, itemId)}/approve`),

  rejectItem: (agentId: string, itemId: string, note?: string) =>
    gateway.post<Item>(`${itemPath(agentId, itemId)}/reject`, { note }),

  reverifyItem: (agentId: string, itemId: string) =>
    gateway.post<Item>(`${itemPath(agentId, itemId)}/reverify`),

  deleteItem: (agentId: string, itemId: string) =>
    gateway.del<DeleteItemOutcome>(itemPath(agentId, itemId)),

  listNodes: (agentId: string, filters: NodeFilters = {}) =>
    gateway.get<NodeListResponse>(`${nodesPath(agentId)}${nodesQuery(filters)}`),

  getNode: (agentId: string, nodeId: string) =>
    gateway.get<NodeDetailResponse>(nodePath(agentId, nodeId)),

  approveNode: (agentId: string, nodeId: string) =>
    gateway.post<DeepNode>(`${nodePath(agentId, nodeId)}/approve`),

  rejectNode: (agentId: string, nodeId: string, note?: string) =>
    gateway.post<DeepNode>(`${nodePath(agentId, nodeId)}/reject`, { note }),

  deleteNode: (agentId: string, nodeId: string) =>
    gateway.del<DeleteNodeOutcome>(nodePath(agentId, nodeId)),
};
