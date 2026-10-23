import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { modelsApi } from "@/lib/api/models";

export function useModelHealth() {
  return useQuery({ queryKey: ["model-health"], queryFn: modelsApi.health, staleTime: 15_000 });
}

export function useModelAliases() {
  return useQuery({ queryKey: ["model-aliases"], queryFn: modelsApi.aliases, staleTime: 30_000 });
}

export function useResetModel() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (agentId?: string) => modelsApi.reset(agentId), onSuccess: () => { qc.invalidateQueries({ queryKey: ["model-health"] }); } });
}
