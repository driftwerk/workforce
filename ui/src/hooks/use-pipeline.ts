import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { pipelinesApi } from "@/lib/api/pipelines";

function status(err: unknown): number | undefined {
  return (err as { status?: number })?.status;
}

const noRetryOn = (count: number, err: unknown) => {
  const s = status(err);
  return s !== 503 && s !== 403 && s !== 401 && s !== 404 && count < 2;
};

/**
 * The app turns focus refetches off by default (most pages are forms); a
 * board that claims to be live has to catch up the moment it is looked at
 * again, and keep its poll going while the tab is in the background, or a
 * tab left open for an hour shows the pipeline as it was an hour ago until
 * someone reloads it.
 */
const live = { refetchOnWindowFocus: true, refetchIntervalInBackground: true } as const;

/** Every pipeline this login may see, with counts. */
export function usePipelineCatalogue() {
  return useQuery({
    queryKey: ["pipelines"],
    queryFn: pipelinesApi.catalogue,
    staleTime: 10_000,
    refetchInterval: 15_000,
    retry: noRetryOn,
    ...live,
  });
}

/** One pipeline's definition. Changes rarely; refreshed on focus and every minute. */
export function usePipeline(scope: string | undefined, id: string | undefined) {
  return useQuery({
    queryKey: ["pipeline", scope, id],
    enabled: Boolean(scope && id),
    queryFn: () => pipelinesApi.get(scope!, id!),
    staleTime: 30_000,
    refetchInterval: 60_000,
    retry: noRetryOn,
    ...live,
  });
}

/**
 * The board to keep on screen while the board for `scope/id` loads: the
 * previous one while the same pipeline's windows or replay change, never
 * another pipeline's, which the page stays mounted across.
 */
export function sameBoard(scope: string | undefined, id: string | undefined) {
  return <T>(previous: T | undefined, query: { queryKey: readonly unknown[] } | undefined): T | undefined =>
    query?.queryKey[1] === scope && query?.queryKey[2] === id ? previous : undefined;
}

/**
 * What a pipeline's board shows under the `windows` asked for. While
 * `polling`, it is refreshed every few seconds — a poll, not a feed: the
 * board diffs snapshots to animate a move, and five seconds is well inside
 * the minutes a stage takes. A replay's history does not change under its
 * clock, so it is read once.
 */
export function usePipelineItems(scope: string | undefined, id: string | undefined, windows: string | undefined, polling: boolean) {
  return useQuery({
    queryKey: ["pipeline-items", scope, id, windows ?? ""],
    enabled: Boolean(scope && id),
    queryFn: () => pipelinesApi.board(scope!, id!, windows),
    placeholderData: sameBoard(scope, id),
    staleTime: polling ? 4_000 : Infinity,
    refetchInterval: polling ? 5_000 : false,
    retry: noRetryOn,
    ...live,
  });
}

/**
 * What one item has cost so far, fetched while its panel is mounted.
 * `updatedAt` is part of the key, so the item moving on the board refetches
 * it; nothing polls.
 */
export function usePipelineItemCosts(scope: string, id: string, item: string, updatedAt: string | undefined) {
  return useQuery({
    queryKey: ["pipeline-item-costs", scope, id, item, updatedAt],
    queryFn: () => pipelinesApi.itemCosts(scope, id, item),
    staleTime: 30_000,
    placeholderData: keepPreviousData,
    retry: noRetryOn,
  });
}

/**
 * How long one item has taken so far, keyed like its costs. A run finishing
 * changes the split between work and waiting without the record moving, so
 * it refetches every minute while a run is working the item.
 */
export function usePipelineItemTimings(scope: string, id: string, item: string, updatedAt: string | undefined) {
  return useQuery({
    queryKey: ["pipeline-item-timings", scope, id, item, updatedAt],
    queryFn: () => pipelinesApi.itemTimings(scope, id, item),
    staleTime: 30_000,
    placeholderData: keepPreviousData,
    retry: noRetryOn,
    refetchInterval: (query) => (query.state.data?.total.working ? 60_000 : false),
  });
}
