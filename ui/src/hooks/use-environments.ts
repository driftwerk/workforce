import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { invalidateAgentConfig } from "@/hooks/use-agents";
import {
  environmentsApi,
  type AgentEnvironments,
  type EnvironmentsBody,
} from "@/lib/api/environments";

const key = (agentId: string | undefined) => ["agent-environments", agentId];

export function useAgentEnvironments(agentId: string | undefined) {
  return useQuery<AgentEnvironments>({
    queryKey: key(agentId),
    queryFn: () => environmentsApi.get(agentId!),
    enabled: !!agentId,
    staleTime: 15_000,
  });
}

/** Replaces the agent's environments table. Settling refetches everything read
 *  from config.toml, so a save refused for a stale ETag leaves the current table on screen. */
export function useSaveAgentEnvironments(agentId: string) {
  const qc = useQueryClient();
  return useMutation<AgentEnvironments, Error, { etag: string; body: EnvironmentsBody }>({
    mutationFn: ({ etag, body }) => environmentsApi.put(agentId, etag, body),
    onSuccess: (saved) => qc.setQueryData(key(agentId), saved),
    onSettled: () => invalidateAgentConfig(qc, agentId),
  });
}
