import { useQuery } from "@tanstack/react-query";
import { statsApi, type StatsPage, type StatsRange } from "@/lib/api/stats";

const STALE_MS = 30_000;

export function useStatsOverview(range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "overview", range],
    queryFn: () => statsApi.overview(range),
    staleTime: STALE_MS,
  });
}

export function useCostBreakdown(range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "cost-breakdown", range],
    queryFn: () => statsApi.costBreakdown(range),
    staleTime: STALE_MS,
  });
}

export function useStatsIdentities(range: StatsRange, page: StatsPage = {}) {
  return useQuery({
    queryKey: ["stats", "identities", range, page],
    queryFn: () => statsApi.identities(range, page),
    staleTime: STALE_MS,
  });
}

export function useIdentityStats(id: string | undefined, range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "identity", id, range],
    queryFn: () => statsApi.identity(id!, range),
    enabled: !!id,
    staleTime: STALE_MS,
  });
}

export function useIdentityTasks(
  id: string | undefined,
  range: StatsRange,
  page: StatsPage = {},
) {
  return useQuery({
    queryKey: ["stats", "identity-tasks", id, range, page],
    queryFn: () => statsApi.identityTasks(id!, range, page),
    enabled: !!id,
    staleTime: STALE_MS,
  });
}

export function useStatsAgents(range: StatsRange, page: StatsPage = {}) {
  return useQuery({
    queryKey: ["stats", "agents", range, page],
    queryFn: () => statsApi.agents(range, page),
    staleTime: STALE_MS,
  });
}

export function useAgentStats(id: string | undefined, range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "agent", id, range],
    queryFn: () => statsApi.agent(id!, range),
    enabled: !!id,
    staleTime: STALE_MS,
  });
}

export function useAgentTasks(
  id: string | undefined,
  range: StatsRange,
  page: StatsPage = {},
) {
  return useQuery({
    queryKey: ["stats", "agent-tasks", id, range, page],
    queryFn: () => statsApi.agentTasks(id!, range, page),
    enabled: !!id,
    staleTime: STALE_MS,
  });
}

export function useTaskTrace(taskId: string | undefined) {
  return useQuery({
    queryKey: ["stats", "task-trace", taskId],
    queryFn: () => statsApi.taskTrace(taskId!),
    enabled: !!taskId,
    staleTime: STALE_MS,
  });
}

export function useStatsModels(range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "models", range],
    queryFn: () => statsApi.models(range),
    staleTime: STALE_MS,
  });
}

export function useStatsTools(range: StatsRange, limit?: number) {
  return useQuery({
    queryKey: ["stats", "tools", range, limit],
    queryFn: () => statsApi.tools(range, limit),
    staleTime: STALE_MS,
  });
}

export function useStatsEfficiency(range: StatsRange) {
  return useQuery({
    queryKey: ["stats", "efficiency", range],
    queryFn: () => statsApi.efficiency(range),
    staleTime: STALE_MS,
  });
}
