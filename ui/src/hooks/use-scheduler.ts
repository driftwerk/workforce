import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { schedulerApi } from "@/lib/api/scheduler";
import type { UpdateJobRequest } from "@/lib/api/scheduler";

export function useScheduledJobs() {
  return useQuery({
    queryKey: ["scheduler"],
    queryFn: schedulerApi.list,
    staleTime: 15_000,
  });
}

export function useCreateJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: schedulerApi.create,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["scheduler"] }),
  });
}

export function useUpdateJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: UpdateJobRequest }) =>
      schedulerApi.update(id, data),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["scheduler"] }),
  });
}

export function useDeleteJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: schedulerApi.delete,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["scheduler"] }),
  });
}

export function useTriggerJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: schedulerApi.trigger,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["scheduler"] }),
  });
}

export function useJobHistory(id: string | undefined) {
  return useQuery({
    queryKey: ["scheduler-history", id],
    queryFn: () => schedulerApi.jobHistory(id!),
    enabled: !!id,
    staleTime: 10_000,
  });
}
