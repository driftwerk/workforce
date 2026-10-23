import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ownedSkillsApi,
  type OwnedSkillKind,
  type SkillVisibility,
} from "@/lib/api/owned-skills";

export function useOwnedSkills<K extends OwnedSkillKind>(kind: K) {
  return useQuery({
    queryKey: [kind],
    queryFn: () => ownedSkillsApi.list(kind),
    staleTime: 30_000,
  });
}

export function useSetSkillVisibility(kind: OwnedSkillKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      name,
      visibility,
    }: {
      name: string;
      visibility: SkillVisibility;
    }) => ownedSkillsApi.setVisibility(kind, name, visibility),
    onSuccess: () => qc.invalidateQueries({ queryKey: [kind] }),
  });
}

export function useUninstallOwnedSkill(kind: OwnedSkillKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => ownedSkillsApi.uninstall(kind, name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["skill-catalog"] });
      qc.invalidateQueries({ queryKey: ["global-skills"] });
      return qc.invalidateQueries({ queryKey: [kind] });
    },
  });
}

/** Re-read the list after the shared share dialog writes through its own key. */
export function useRefreshOwnedSkills(kind: OwnedSkillKind) {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: [kind] });
}
