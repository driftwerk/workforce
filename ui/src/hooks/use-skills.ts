import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { skillsApi } from "@/lib/api/skills";

export function useGlobalSkills() {
  return useQuery({
    queryKey: ["global-skills"],
    queryFn: () => skillsApi.list(),
    staleTime: 30_000,
  });
}

export function useGlobalSkillContent(name: string | null) {
  return useQuery({
    queryKey: ["global-skill", name],
    queryFn: () => skillsApi.get(name!),
    enabled: !!name,
    staleTime: 10_000,
  });
}

export function useSaveGlobalSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, content }: { name: string; content: string }) =>
      skillsApi.put(name, content),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["global-skill", vars.name] });
      qc.invalidateQueries({ queryKey: ["global-skills"] });
      qc.invalidateQueries({ queryKey: ["external-skills"] });
      qc.invalidateQueries({ queryKey: ["skill-catalog"] });
    },
  });
}

export function useDeleteGlobalSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => skillsApi.delete(name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["global-skills"] });
      qc.invalidateQueries({ queryKey: ["external-skills"] });
      qc.invalidateQueries({ queryKey: ["skill-catalog"] });
    },
  });
}

export function useGlobalSkillConfig(name: string | null) {
  return useQuery({
    queryKey: ["global-skill-config", name],
    queryFn: () => skillsApi.getConfig(name!),
    enabled: !!name,
    staleTime: 10_000,
  });
}

export function useSaveGlobalSkillConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, content }: { name: string; content: string }) =>
      skillsApi.putConfig(name, content),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["global-skill-config", vars.name] });
    },
  });
}

export function useSkillCatalog() {
  return useQuery({
    queryKey: ["skill-catalog"],
    queryFn: () => skillsApi.catalog(),
    staleTime: 60_000,
  });
}

export function useInstallFromCatalog() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ catalog, name }: { catalog: string; name: string }) =>
      skillsApi.installFromCatalog(catalog, name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["global-skills"] });
      qc.invalidateQueries({ queryKey: ["external-skills"] });
      qc.invalidateQueries({ queryKey: ["wasm-skills"] });
      return qc.invalidateQueries({ queryKey: ["skill-catalog"] });
    },
  });
}
