import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { selfLearningApi } from "@/lib/api/self-learning";
import type {
  DeepNode,
  DeleteItemOutcome,
  DeleteNodeOutcome,
  DeletePlanOutcome,
  Item,
  ItemFilters,
  ItemListResponse,
  NodeDetailResponse,
  NodeFilters,
  NodeListResponse,
  Plan,
  PlanListResponse,
  PlanUpsert,
  RunPlanOutcome,
} from "@/lib/api/self-learning";

export type {
  CurationMode,
  DeepNode,
  DeleteItemOutcome,
  DeleteNodeOutcome,
  DeletePlanOutcome,
  Item,
  ItemFilters,
  ItemSource,
  ItemStatus,
  NodeCoverage,
  NodeDetailResponse,
  NodeFilters,
  NodeSource,
  NodeStatus,
  Plan,
  PlanEstimate,
  PlanSource,
  PlanUpsert,
  PlanUpsertSource,
  RunPlanOutcome,
} from "@/lib/api/self-learning";

const PLANS_KEY = "self-learning-plans";
const ITEMS_KEY = "self-learning-items";
const NODES_KEY = "deep-expertise-nodes";

export function useAgentPlans(agentId: string | undefined) {
  return useQuery<PlanListResponse>({
    queryKey: [PLANS_KEY, agentId],
    queryFn: () => selfLearningApi.listPlans(agentId!),
    enabled: !!agentId,
    staleTime: 15_000,
  });
}

export function useAgentItems(
  agentId: string | undefined,
  filters: ItemFilters,
) {
  return useQuery<ItemListResponse>({
    queryKey: [ITEMS_KEY, agentId, filters],
    queryFn: () => selfLearningApi.listItems(agentId!, filters),
    enabled: !!agentId,
    staleTime: 10_000,
  });
}

export function useUpsertPlan(agentId: string) {
  const qc = useQueryClient();
  return useMutation<Plan, Error, PlanUpsert>({
    mutationFn: (plan) => selfLearningApi.upsertPlan(agentId, plan),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [PLANS_KEY, agentId] });
    },
  });
}

export function useDeletePlan(agentId: string) {
  const qc = useQueryClient();
  return useMutation<DeletePlanOutcome, Error, string>({
    mutationFn: (planId) => selfLearningApi.deletePlan(agentId, planId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [PLANS_KEY, agentId] });
      // Deleting a plan orphans its items; refresh every filtered item view.
      qc.invalidateQueries({ queryKey: [ITEMS_KEY, agentId] });
    },
  });
}

export function useRunPlan(agentId: string) {
  const qc = useQueryClient();
  return useMutation<RunPlanOutcome, Error, string>({
    mutationFn: (planId) => selfLearningApi.runPlan(agentId, planId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [PLANS_KEY, agentId] });
    },
  });
}

// Every item-status change shifts a plan's item counts, so each item mutation
// invalidates both the item lists (all filter variants) and the plans list.
function useItemMutation<T>(
  agentId: string,
  fn: (itemId: string) => Promise<T>,
) {
  const qc = useQueryClient();
  return useMutation<T, Error, string>({
    mutationFn: (itemId) => fn(itemId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [ITEMS_KEY, agentId] });
      qc.invalidateQueries({ queryKey: [PLANS_KEY, agentId] });
    },
  });
}

export function useApproveItem(agentId: string) {
  return useItemMutation<Item>(agentId, (itemId) =>
    selfLearningApi.approveItem(agentId, itemId),
  );
}

export function useReverifyItem(agentId: string) {
  return useItemMutation<Item>(agentId, (itemId) =>
    selfLearningApi.reverifyItem(agentId, itemId),
  );
}

export function useDeleteItem(agentId: string) {
  return useItemMutation<DeleteItemOutcome>(agentId, (itemId) =>
    selfLearningApi.deleteItem(agentId, itemId),
  );
}

export function useRejectItem(agentId: string) {
  const qc = useQueryClient();
  return useMutation<Item, Error, { itemId: string; note?: string }>({
    mutationFn: ({ itemId, note }) =>
      selfLearningApi.rejectItem(agentId, itemId, note),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [ITEMS_KEY, agentId] });
      qc.invalidateQueries({ queryKey: [PLANS_KEY, agentId] });
    },
  });
}

// ── deep-expertise tree ──────────────────────────────────────────────

export function useAgentNodes(
  agentId: string | undefined,
  filters: NodeFilters,
) {
  return useQuery<NodeListResponse>({
    queryKey: [NODES_KEY, agentId, filters],
    queryFn: () => selfLearningApi.listNodes(agentId!, filters),
    enabled: !!agentId,
    staleTime: 10_000,
  });
}

export function useAgentNode(
  agentId: string | undefined,
  nodeId: string | undefined,
  enabled: boolean,
) {
  return useQuery<NodeDetailResponse>({
    queryKey: [NODES_KEY, agentId, "detail", nodeId],
    queryFn: () => selfLearningApi.getNode(agentId!, nodeId!),
    enabled: !!agentId && !!nodeId && enabled,
    staleTime: 10_000,
  });
}

// Every node-status change can shift the tree, so each mutation invalidates the
// whole node-query family for the agent (the cached roots + every fetched
// subtree), mirroring `useItemMutation`.
function useNodeMutation<T>(
  agentId: string,
  fn: (nodeId: string) => Promise<T>,
) {
  const qc = useQueryClient();
  return useMutation<T, Error, string>({
    mutationFn: (nodeId) => fn(nodeId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [NODES_KEY, agentId] });
    },
  });
}

export function useApproveNode(agentId: string) {
  return useNodeMutation<DeepNode>(agentId, (nodeId) =>
    selfLearningApi.approveNode(agentId, nodeId),
  );
}

export function useDeleteNode(agentId: string) {
  return useNodeMutation<DeleteNodeOutcome>(agentId, (nodeId) =>
    selfLearningApi.deleteNode(agentId, nodeId),
  );
}

export function useRejectNode(agentId: string) {
  const qc = useQueryClient();
  return useMutation<DeepNode, Error, { nodeId: string; note?: string }>({
    mutationFn: ({ nodeId, note }) =>
      selfLearningApi.rejectNode(agentId, nodeId, note),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [NODES_KEY, agentId] });
    },
  });
}
