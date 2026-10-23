import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { budgetApi, type PutOverrideBody } from "@/lib/api/budget";

export function useBudgetStatus() {
  return useQuery({
    queryKey: ["budget-status"],
    queryFn: budgetApi.status,
    // Spend figures move as agents work; the governor itself caches ~30s.
    refetchInterval: 30_000,
    staleTime: 15_000,
  });
}

export function usePutOverride() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: PutOverrideBody) => budgetApi.putOverride(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["budget-status"] }),
  });
}

export function useClearOverride() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ scope, key }: { scope: string; key: string }) =>
      budgetApi.clearOverride(scope, key),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["budget-status"] }),
  });
}
