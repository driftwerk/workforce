import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { monitorsApi } from "@/lib/api/monitors";

export function useMonitors() {
  return useQuery({ queryKey: ["monitors"], queryFn: monitorsApi.list, staleTime: 30_000 });
}

export function useMonitorSource(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["monitor-source", id, owner ?? null],
    queryFn: () => monitorsApi.getSource(id!, owner),
    enabled: !!id,
    staleTime: 60_000,
  });
}

export function useMonitorActivity(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["monitor-activity", id, owner ?? null],
    queryFn: () => monitorsApi.activity(id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useSaveMonitor() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      toml,
      from,
      visibility,
      owner,
    }: {
      id: string;
      toml: string;
      from?: string;
      visibility?: string;
      owner?: string | null;
    }) => monitorsApi.save(id, toml, from, visibility, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["monitors"] });
      qc.invalidateQueries({ queryKey: ["monitor-source", vars.id] });
      if (vars.from && vars.from !== vars.id) {
        qc.invalidateQueries({ queryKey: ["monitor-source", vars.from] });
      }
    },
  });
}

export function useMonitorRevisions(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["monitor-revisions", id, owner ?? null],
    queryFn: () => monitorsApi.listRevisions(id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useRestoreMonitorRevision() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, rev, owner }: { id: string; rev: number; owner?: string | null }) =>
      monitorsApi.restoreRevision(id, rev, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["monitors"] });
      qc.invalidateQueries({ queryKey: ["monitor-source", vars.id] });
      qc.invalidateQueries({ queryKey: ["monitor-revisions", vars.id] });
    },
  });
}

export function useDeleteMonitor() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, owner }: { id: string; owner?: string | null }) =>
      monitorsApi.del(id, owner),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["monitors"] });
    },
  });
}
