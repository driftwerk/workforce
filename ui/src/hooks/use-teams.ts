import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { teamsApi } from "@/lib/api/teams";

export function useIdentityTeams() {
  return useQuery({
    queryKey: ["identity-teams"],
    queryFn: teamsApi.listIdentityTeams,
    staleTime: 30_000,
  });
}

export function useIdentityTeam(id: string | undefined) {
  return useQuery({
    queryKey: ["identity-team", id],
    queryFn: () => teamsApi.getIdentityTeam(id!),
    enabled: !!id,
    staleTime: 30_000,
  });
}

export function useCreateIdentityTeam() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: teamsApi.createIdentityTeam,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-teams"] }),
  });
}

export function useDeleteIdentityTeam() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: teamsApi.deleteIdentityTeam,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-teams"] }),
  });
}

export function useTeamMutation() {
  const qc = useQueryClient();
  return {
    addMember: useMutation({
      mutationFn: ({ teamId, identityId }: { teamId: string; identityId: string }) =>
        teamsApi.addMember(teamId, identityId),
      onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-team"] }),
    }),
    removeMember: useMutation({
      mutationFn: ({ teamId, memberId }: { teamId: string; memberId: string }) =>
        teamsApi.removeMember(teamId, memberId),
      onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-team"] }),
    }),
    approveJoin: useMutation({
      mutationFn: ({ teamId, applicantId }: { teamId: string; applicantId: string }) =>
        teamsApi.approveJoin(teamId, applicantId),
      onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-team"] }),
    }),
    rejectJoin: useMutation({
      mutationFn: ({ teamId, applicantId }: { teamId: string; applicantId: string }) =>
        teamsApi.rejectJoin(teamId, applicantId),
      onSuccess: () => qc.invalidateQueries({ queryKey: ["identity-team"] }),
    }),
  };
}
