import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { sessionsApi } from "@/lib/api/sessions";

export function useSessions(agentId?: string, conversationalOnly = false) {
  return useQuery({
    queryKey: ["sessions", agentId, conversationalOnly],
    queryFn: () => sessionsApi.list(agentId, undefined, conversationalOnly),
    staleTime: 10_000,
  });
}

/** Backward-paginating cursor into the transcript: the oldest entry currently
 *  loaded. `undefined` for the first page (server returns the most recent). */
type TranscriptCursor = { before: string; beforeSession: string } | undefined;

/** Transcript pages load newest-first (one page = the latest 50 messages),
 *  then walk backward through the rolled-over thread chain via "Load earlier".
 *  Each page is internally chronological; render `pages` reversed so the whole
 *  conversation reads oldest → newest. */
export function useSessionTranscript(id: string | undefined) {
  return useInfiniteQuery({
    queryKey: ["session-transcript", id],
    enabled: !!id,
    staleTime: 5_000,
    initialPageParam: undefined as TranscriptCursor,
    queryFn: ({ pageParam }) =>
      sessionsApi.transcript(id!, {
        before: pageParam?.before,
        beforeSession: pageParam?.beforeSession,
      }),
    getNextPageParam: (lastPage): TranscriptCursor => {
      const oldest = lastPage.entries[0];
      if (!lastPage.has_more || !oldest) return undefined;
      return { before: oldest.id, beforeSession: oldest.session_id };
    },
  });
}

export function useSessionStats(id: string | undefined) {
  return useQuery({ queryKey: ["session-stats", id], queryFn: () => sessionsApi.stats(id!), enabled: !!id, staleTime: 10_000 });
}
