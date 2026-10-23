import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { heartbeatApi } from "@/lib/api/heartbeat";
import type { CreateHeartbeatCheckRequest, UpdateHeartbeatCheckRequest } from "@/lib/api/heartbeat";

export function useHeartbeatStatus() {
  return useQuery({
    queryKey: ["heartbeat"],
    queryFn: heartbeatApi.status,
    staleTime: 15_000,
  });
}

export function useHeartbeatChecks() {
  return useQuery({
    queryKey: ["heartbeat-checks"],
    queryFn: heartbeatApi.listChecks,
    staleTime: 15_000,
  });
}

export function useHeartbeatWake() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: heartbeatApi.wake,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["heartbeat"] }),
  });
}

export function useHeartbeatCheckDetail(id: string | undefined) {
  return useQuery({
    queryKey: ["heartbeat-check", id],
    queryFn: () => heartbeatApi.getCheck(id!),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useCreateHeartbeatCheck() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (data: CreateHeartbeatCheckRequest) => heartbeatApi.createCheck(data),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["heartbeat-checks"] }),
  });
}

export function useUpdateHeartbeatCheck() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: UpdateHeartbeatCheckRequest }) =>
      heartbeatApi.updateCheck(id, data),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["heartbeat-checks"] }),
  });
}

export function useDeleteHeartbeatCheck() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => heartbeatApi.deleteCheck(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["heartbeat-checks"] }),
  });
}

export function useTriggerHeartbeatCheck() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => heartbeatApi.triggerCheck(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["heartbeat-checks"] }),
  });
}

export function useHeartbeatHistory() {
  return useQuery({
    queryKey: ["heartbeat-history"],
    queryFn: heartbeatApi.history,
    staleTime: 10_000,
  });
}
