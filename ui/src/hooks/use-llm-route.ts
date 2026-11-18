import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { llmRouteApi, type AgentRoute, type PersonalRoute } from "@/lib/api/llm-route";

const MINE = ["my-llm-route"];
const agentKey = (agentId: string) => ["my-agent-llm-route", agentId];

export function useMyRoute() {
  return useQuery({ queryKey: MINE, queryFn: llmRouteApi.mine, staleTime: 10_000 });
}

export function useMyAgentRoute(agentId: string) {
  return useQuery({
    queryKey: agentKey(agentId),
    queryFn: () => llmRouteApi.agent(agentId),
    staleTime: 10_000,
  });
}

/** Sets the every-agent route; every agent's view follows it. */
export function useSetMyRoute() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (route: PersonalRoute | null) => llmRouteApi.setMine(route),
    onSuccess: (view) => {
      qc.setQueryData(MINE, view);
      qc.invalidateQueries({ queryKey: ["my-agent-llm-route"] });
    },
  });
}

export function useSetMyAgentRoute(agentId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (route: AgentRoute | null) => llmRouteApi.setAgent(agentId, route),
    onSuccess: (view) => qc.setQueryData(agentKey(agentId), view),
  });
}
