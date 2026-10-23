import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { knowHowApi } from "@/lib/api/know-how";
import type {
  KnowHowActionOutcome,
  KnowHowEraseOutcome,
  KnowHowListResponse,
} from "@/lib/api/know-how";

export type {
  KnowHowActionOutcome,
  KnowHowEntry,
  KnowHowEraseOutcome,
  KnowHowKind,
  KnowHowListResponse,
  KnowHowStatus,
} from "@/lib/api/know-how";

const KNOW_HOW_KEY = "agent-know-how";

export function useAgentKnowHow(agentId: string | undefined) {
  return useQuery<KnowHowListResponse>({
    queryKey: [KNOW_HOW_KEY, agentId],
    queryFn: () => knowHowApi.list(agentId!),
    enabled: !!agentId,
    staleTime: 15_000,
  });
}

function useEntryMutation(
  agentId: string,
  fn: (id: number) => Promise<KnowHowActionOutcome>,
) {
  const qc = useQueryClient();
  return useMutation<KnowHowActionOutcome, Error, number>({
    mutationFn: fn,
    onSuccess: () => qc.invalidateQueries({ queryKey: [KNOW_HOW_KEY, agentId] }),
  });
}

export function usePinKnowHow(agentId: string) {
  return useEntryMutation(agentId, (id) => knowHowApi.pin(agentId, id));
}

export function useRetireKnowHow(agentId: string) {
  return useEntryMutation(agentId, (id) => knowHowApi.retire(agentId, id));
}

export function useDeleteKnowHow(agentId: string) {
  return useEntryMutation(agentId, (id) => knowHowApi.remove(agentId, id));
}

/** Erase one identity's contributions on every agent. */
export function useEraseKnowHow() {
  const qc = useQueryClient();
  return useMutation<KnowHowEraseOutcome, Error, string>({
    mutationFn: knowHowApi.eraseIdentity,
    onSuccess: () => qc.invalidateQueries({ queryKey: [KNOW_HOW_KEY] }),
  });
}
