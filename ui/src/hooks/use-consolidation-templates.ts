import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { consolidationTemplatesApi } from "@/lib/api/consolidation-templates";

export function useConsolidationTemplates() {
  return useQuery({
    queryKey: ["consolidation-templates"],
    queryFn: consolidationTemplatesApi.list,
    staleTime: 30_000,
  });
}

export function useConsolidationTemplateSource(
  id: string | undefined,
  owner?: string | null,
) {
  return useQuery({
    queryKey: ["consolidation-template-source", id, owner ?? null],
    queryFn: () => consolidationTemplatesApi.getSource(id!, owner),
    enabled: !!id,
    staleTime: 60_000,
  });
}

export function useSaveConsolidationTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      json,
      from,
      visibility,
    }: {
      id: string;
      json: string;
      from?: string;
      visibility?: string;
    }) => consolidationTemplatesApi.save(id, json, from, visibility),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["consolidation-templates"] });
      qc.invalidateQueries({
        queryKey: ["consolidation-template-source", vars.id],
      });
      // On a rename the old id's cached source is now stale.
      if (vars.from && vars.from !== vars.id) {
        qc.invalidateQueries({
          queryKey: ["consolidation-template-source", vars.from],
        });
      }
    },
  });
}

export function useConsolidationTemplateRevisions(
  id: string | undefined,
  owner?: string | null,
) {
  return useQuery({
    queryKey: ["consolidation-template-revisions", id, owner ?? null],
    queryFn: () => consolidationTemplatesApi.listRevisions(id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useRestoreConsolidationTemplateRevision() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      rev,
      owner,
    }: {
      id: string;
      rev: number;
      owner?: string | null;
    }) => consolidationTemplatesApi.restoreRevision(id, rev, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["consolidation-templates"] });
      qc.invalidateQueries({
        queryKey: ["consolidation-template-source", vars.id],
      });
      qc.invalidateQueries({
        queryKey: ["consolidation-template-revisions", vars.id],
      });
    },
  });
}

export function useDeleteConsolidationTemplate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => consolidationTemplatesApi.del(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["consolidation-templates"] });
    },
  });
}
