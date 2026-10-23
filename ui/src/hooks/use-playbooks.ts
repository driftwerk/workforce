import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { playbooksApi } from "@/lib/api/playbooks";

export function usePlaybooks() {
  return useQuery({ queryKey: ["playbooks"], queryFn: playbooksApi.list, staleTime: 30_000 });
}

export function usePlaybookSource(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["playbook-source", id, owner ?? null],
    queryFn: () => playbooksApi.getSource(id!, owner),
    enabled: !!id,
    staleTime: 60_000,
  });
}

export function useSavePlaybook() {
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
    }) => playbooksApi.save(id, toml, from, visibility, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["playbooks"] });
      qc.invalidateQueries({ queryKey: ["playbook-source", vars.id] });
      // On a rename the old id's cached source is now stale.
      if (vars.from && vars.from !== vars.id) {
        qc.invalidateQueries({ queryKey: ["playbook-source", vars.from] });
      }
    },
  });
}

export function usePlaybookRevisions(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["playbook-revisions", id, owner ?? null],
    queryFn: () => playbooksApi.listRevisions(id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useRestorePlaybookRevision() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, rev, owner }: { id: string; rev: number; owner?: string | null }) =>
      playbooksApi.restoreRevision(id, rev, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["playbooks"] });
      qc.invalidateQueries({ queryKey: ["playbook-source", vars.id] });
      qc.invalidateQueries({ queryKey: ["playbook-revisions", vars.id] });
    },
  });
}

export function useDeletePlaybook() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, owner }: { id: string; owner?: string | null }) =>
      playbooksApi.del(id, owner),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["playbooks"] });
    },
  });
}
