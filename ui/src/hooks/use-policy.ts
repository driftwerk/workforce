import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { policyApi } from "@/lib/api/policy";

export function usePolicyRules() {
  return useQuery({
    queryKey: ["policy-rules"],
    queryFn: policyApi.listRules,
    staleTime: 30_000,
  });
}

export function useCreateRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: policyApi.createRule,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["policy-rules"] }),
  });
}

export function useUpdateRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, ...data }: { name: string } & Partial<Parameters<typeof policyApi.updateRule>[1]>) =>
      policyApi.updateRule(name, data),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["policy-rules"] }),
  });
}

export function useDeleteRule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: policyApi.deleteRule,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["policy-rules"] }),
  });
}

export function usePolicyProposals(status?: string) {
  return useQuery({
    queryKey: ["policy-proposals", status],
    queryFn: () => policyApi.listProposals(status),
    staleTime: 15_000,
  });
}

export function useApproveProposal() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, note }: { id: string; note?: string }) =>
      policyApi.approveProposal(id, note),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["policy-proposals"] }),
  });
}

export function useRejectProposal() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, note }: { id: string; note?: string }) =>
      policyApi.rejectProposal(id, note),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["policy-proposals"] }),
  });
}
