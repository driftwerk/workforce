import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router";
import { AlertCircle, ArrowLeft, Workflow } from "lucide-react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useNow } from "@/hooks/use-now";
import { usePipeline, usePipelineItems } from "@/hooks/use-pipeline";
import { usePipelineBoardPrefs } from "@/hooks/use-pipeline-board-prefs";
import { Board, type Item } from "@/components/pipeline/board";
import { Feed } from "@/components/pipeline/feed";
import { DefinitionPanel } from "@/components/pipeline/definition-panel";
import { GuidePanel } from "@/components/pipeline/guide-panel";
import { ItemPanel } from "@/components/pipeline/item-panel";
import { Notices } from "@/components/pipeline/notices";
import { ReplayBar, type ReplayState } from "@/components/pipeline/replay-bar";
import { Stat } from "@/components/pipeline/stat";
import type { ColumnWindow } from "@/lib/api/pipelines";
import { cardsPerColumn, hasWindows, windowsParam } from "@/lib/pipeline/columns";
import type { PipelineDefinition } from "@/lib/pipeline/definition";
import { allEvents } from "@/lib/pipeline/lineage";
import { boardStats } from "@/lib/pipeline/stats";

const NO_WINDOWS: Record<string, ColumnWindow> = {};

function Stats({ def, items, hidden, at }: { def: PipelineDefinition; items: Item[]; hidden: Record<string, number>; at: Date | null }) {
  const now = useNow(at === null);
  const { inFlow, done, waiting, healing, halted, moved } = useMemo(
    () => boardStats(def, items, hidden, at, now),
    [def, items, hidden, at, now],
  );
  return (
    <div className="grid gap-3 sm:grid-cols-3 xl:grid-cols-6">
      <Stat label="In flow" value={inFlow} />
      <Stat label={def.lanes[def.lanes.length - 1]?.label ?? "Done"} value={done} tone="good" />
      <Stat label="Needs a person" value={waiting} tone={waiting ? "warn" : undefined} />
      <Stat label="Self-healing" value={healing} tone={healing ? "healing" : undefined} />
      <Stat label="Halted" value={halted} tone={halted ? "bad" : undefined} />
      <Stat label="Moved in 24h" value={moved} />
    </div>
  );
}

export default function PipelineBoardPage() {
  const { scope, id } = useParams();
  const pipeline = usePipeline(scope, id);
  const { prefs, setCards, setWindow } = usePipelineBoardPrefs(scope!, id!);
  const [replay, setReplay] = useState<ReplayState>({
    active: false,
    playing: false,
    t: Date.now(),
    from: Date.now() - 24 * 3600 * 1000,
    to: Date.now(),
    speed: 60,
  });
  const records = usePipelineItems(scope, id, windowsParam(prefs.windows, replay.active), !replay.active);

  const def = pipeline.data?.definition ?? null;
  const items: Item[] = useMemo(
    () => (records.data?.items ?? []).map((r) => ({ id: r.item, body: r.record })),
    [records.data],
  );
  const events = useMemo(() => (def ? allEvents(def, items) : []), [def, items]);
  const windows = replay.active ? NO_WINDOWS : (records.data?.windows ?? NO_WINDOWS);
  const hidden = useMemo(
    () => Object.fromEntries(Object.entries(windows).map(([column, applied]) => [column, applied.hidden])),
    [windows],
  );

  const spanFrom = records.data?.span?.from;
  const spanTo = records.data?.span?.to;
  useEffect(() => {
    if (replay.active || !spanFrom || !spanTo) return;
    const to = Math.max(new Date(spanTo).getTime(), Date.now());
    const from = Math.min(new Date(spanFrom).getTime(), to - 3600 * 1000);
    setReplay((r) => (r.from === from && r.to === to ? r : { ...r, from, to, t: to }));
  }, [spanFrom, spanTo, replay.active]);
  const onReplay = useCallback((next: ReplayState) => setReplay(next), []);
  const at = replay.active ? new Date(replay.t) : null;
  const [opened, setOpened] = useState<Item | null>(null);
  const openItem = opened && (items.find((i) => i.id === opened.id) ?? opened);

  if ((pipeline.isLoading && !pipeline.data) || (records.isLoading && !records.data && !pipeline.error)) return <PageSkeleton />;

  const error = pipeline.error ?? records.error;
  const status = (error as { status?: number } | null)?.status;
  const itemLabel = pipeline.data?.item_label ?? "item";
  const nothingHidden = Object.values(windows).every((applied) => applied.hidden === 0);
  const cards = def ? cardsPerColumn(def, prefs.cards) : 0;

  return (
    <TooltipProvider delayDuration={200}>
      <div className="space-y-5 p-6">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <Link to="/pipelines" className="mb-1 inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
              <ArrowLeft className="h-3 w-3" /> All pipelines
            </Link>
            <h1 className="text-2xl font-bold">{pipeline.data?.name ?? id}</h1>
            <p className="text-muted-foreground">
              {pipeline.data?.description ?? `Every ${itemLabel} this pipeline is working, where it is, and how it got there.`}
              {!replay.active && !error && (
                <span className="ml-2 inline-flex items-center gap-1 text-xs text-success">
                  <span className="h-1.5 w-1.5 rounded-full bg-success motion-safe:animate-pulse" aria-hidden />
                  live
                </span>
              )}
            </p>
          </div>
        </div>

        <Notices key={`${scope}/${id}`} scope={scope!} id={id!} notices={pipeline.data?.notices ?? []} />

        {error ? (
          <EmptyState
            icon={AlertCircle}
            title={
              status === 404
                ? "No such pipeline"
                : status === 403
                  ? "This login cannot read that scope"
                  : status === 503
                    ? "The document store is not configured on this hub"
                    : "Could not load the pipeline"
            }
            description={
              status === 404
                ? `Nothing is defined at ${scope}/${id}. Pipelines are listed on the Pipelines page.`
                : String((error as Error).message ?? error)
            }
            action={{ label: "Retry", onClick: () => { pipeline.refetch(); records.refetch(); } }}
          />
        ) : def && items.length === 0 && nothingHidden ? (
          <>
            <EmptyState
              icon={Workflow}
              title={`No ${itemLabel}s recorded yet`}
              description={`Nothing has been recorded in ${scope}/${def.collection}. The first ${itemLabel} appears within seconds of a pipeline.record call.`}
            />
            <GuidePanel key={`${scope}/${id}`} def={def} scope={scope!} id={id!} />
            <DefinitionPanel def={def} scope={scope!} id={id!} version={pipeline.data!.version} />
          </>
        ) : def ? (
          <>
            <Stats def={def} items={items} hidden={hidden} at={at} />
            <ReplayBar state={replay} onChange={onReplay} loading={replay.active && records.isPlaceholderData} />
            <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_320px]">
              <Board
                def={def}
                items={items}
                at={at}
                cards={cards}
                windows={windows}
                selected={prefs.windows}
                busy={!replay.active && records.isPlaceholderData}
                onCards={setCards}
                onWindow={setWindow}
                onOpen={(docId) => setOpened(items.find((i) => i.id === docId) ?? null)}
              />
              <Feed events={events} at={at} />
            </div>
            <ItemPanel
              scope={scope!}
              id={id!}
              def={def}
              itemLabel={itemLabel}
              item={openItem}
              canViewCosts={pipeline.data?.can_view_costs ?? false}
              onClose={() => setOpened(null)}
            />
            <GuidePanel key={`${scope}/${id}`} def={def} scope={scope!} id={id!} />
            <DefinitionPanel def={def} scope={scope!} id={id!} version={pipeline.data!.version} />
            <p className="text-[11px] text-muted-foreground">
              Colour is the tone the definition gives an event: blue in progress, green passed, amber waiting on a
              person, sky self-healing (the pipeline is still working on it; nothing to do), red halted. A note under
              a card's label is the latest annotation since it moved; an amber one also counts as needing a person. Within a
              column, work in progress comes first, then what is due soonest, then the rest, newest first. Each column
              shows {cards} cards before it scrolls
              {hasWindows(def)
                ? "; finished columns show what entered them recently, and their footer says how many older are hidden"
                : ""}
              . Replay folds each {itemLabel}'s recorded history up to the chosen moment.
            </p>
          </>
        ) : null}
      </div>
    </TooltipProvider>
  );
}
