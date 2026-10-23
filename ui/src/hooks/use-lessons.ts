import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  lessonsApi,
  type BackfillBody,
  type CleanupBody,
  type JobStatus,
  type LessonsFilter,
} from "@/lib/api/lessons";

function invalidateCorpus(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["lessons"] });
  qc.invalidateQueries({ queryKey: ["lessons-metrics"] });
  qc.invalidateQueries({ queryKey: ["lessons-recurrences"] });
}

export function useLessons(filter: LessonsFilter) {
  return useQuery({
    queryKey: ["lessons", filter],
    queryFn: () => lessonsApi.list(filter),
    staleTime: 15_000,
  });
}

export function useLessonsMetrics(agent: string, days: number) {
  return useQuery({
    queryKey: ["lessons-metrics", agent, days],
    queryFn: () => lessonsApi.metrics(agent, days),
    staleTime: 30_000,
  });
}

export function useLessonsRecurrences(agent: string) {
  return useQuery({
    queryKey: ["lessons-recurrences", agent],
    queryFn: () => lessonsApi.recurrences(agent),
    staleTime: 30_000,
  });
}

export function useDeleteLesson() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => lessonsApi.remove(id),
    onSuccess: () => invalidateCorpus(qc),
  });
}

export function useCleanupJob() {
  return useMutation({
    mutationFn: (body: CleanupBody) => lessonsApi.cleanup(body),
  });
}

export function useBackfillJob() {
  return useMutation({
    mutationFn: (body: BackfillBody) => lessonsApi.backfill(body),
  });
}

/** Poll a kicked job every ~2s until it leaves `running`. Pass the job id once
 *  the kick mutation returns; pass `null` to disable. */
export function useJobStatus(jobId: string | null) {
  return useQuery({
    queryKey: ["lessons-job", jobId],
    queryFn: () => lessonsApi.jobStatus(jobId as string),
    enabled: !!jobId,
    // react-query v5: the callback receives the Query; keep polling while the
    // last status is "running", then stop.
    refetchInterval: (query) => {
      const data = query.state.data as JobStatus | undefined;
      return data && data.status !== "running" ? false : 2000;
    },
  });
}

/** Invalidate the corpus views once a *real* (non-dry-run) cleanup/backfill job
 *  finishes — it pruned, merged, or wrote lessons, so cards/charts are stale.
 *  Dry runs change nothing and are skipped. */
export function useInvalidateCorpusOnJobDone(job: JobStatus | undefined) {
  const qc = useQueryClient();
  const done =
    job?.status === "done" && "dry_run" in job && job.dry_run === false;
  useEffect(() => {
    if (done) invalidateCorpus(qc);
  }, [done, qc]);
}
