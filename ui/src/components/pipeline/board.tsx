import { type CSSProperties, useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";
import { useNow } from "@/hooks/use-now";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { ColumnWindow } from "@/lib/api/pipelines";
import { Token } from "./token";
import { type Activity, activityAt, byActivity } from "@/lib/pipeline/activity";
import {
  cardChoices,
  cardsPerColumn,
  columnHeight,
  emptyText,
  hasWindows,
  hiddenText,
  windowChoices,
  windowReach,
} from "@/lib/pipeline/columns";
import type { PipelineDefinition, Position } from "@/lib/pipeline/definition";
import { type LineageBody, type StandingAnnotation, enteredAt, itemId, positionAt, standingAnnotation } from "@/lib/pipeline/lineage";

export interface Item {
  id: string;
  body: LineageBody;
}

export interface Placed {
  id: string;
  /** The record's document id, which the item panel looks the item up by. */
  docId: string;
  body: LineageBody;
  position: Position;
  enteredAt?: string;
  /** What is happening to the item at the board's moment; it orders the column. */
  activity: Activity;
  /** The annotation showing on the item at the board's moment. */
  annotation: StandingAnnotation | null;
}

/** Every record placed at time `at` (null = now), in column order; records
 *  not yet in the pipeline at that moment are left out. Leases and due times
 *  are read against `at`, or `now` on the live board. */
export function place(def: PipelineDefinition, items: Item[], at: Date | null, now: Date): Placed[] {
  return items
    .flatMap(({ id, body }): Placed[] => {
      const position = positionAt(def, body, at);
      return position
        ? [
            {
              id: itemId(body, id),
              docId: id,
              body,
              position,
              enteredAt: enteredAt(def, body, at),
              activity: activityAt(def, body, at, now),
              annotation: standingAnnotation(def, body, at),
            },
          ]
        : [];
    })
    .sort(byActivity);
}

/**
 * Which items changed column since the last render, so their token can
 * play the arrival animation. A brief window after a change counts as
 * "arrived".
 */
function useArrivals(placed: Placed[]): Set<string> {
  const previous = useRef<Map<string, string>>(new Map());
  const [arrived, setArrived] = useState<Set<string>>(new Set());
  useEffect(() => {
    const moved = new Set<string>();
    const next = new Map<string, string>();
    for (const { id, position } of placed) {
      next.set(id, position.column);
      const before = previous.current.get(id);
      if (before !== undefined && before !== position.column) moved.add(id);
    }
    previous.current = next;
    if (moved.size === 0) return;
    setArrived(moved);
    const timer = window.setTimeout(() => setArrived(new Set()), 1_200);
    return () => window.clearTimeout(timer);
  }, [placed]);
  return arrived;
}

/** A windowed column's window as the hub applied it, with the viewer's choice. */
interface WindowChoice {
  spec: ColumnWindow;
  /** The viewer's choice, shown at once while it loads. */
  selected: string;
  busy: boolean;
  onChoose: (window: string | undefined) => void;
}

/** How many older items a column's window hides, and the viewer's choice of window; the definition's window clears the choice. */
function WindowFooter({ label, spec, selected, busy, onChoose }: WindowChoice & { label: string }) {
  return (
    <footer className="flex items-center justify-between gap-1 border-t px-2 py-1.5 text-[10px] text-muted-foreground">
      <span aria-live="polite" aria-busy={busy} className="min-w-0 truncate">
        {busy ? "Loading…" : hiddenText(spec.hidden, spec.window)}
      </span>
      <DropdownMenu>
        <DropdownMenuTrigger
          aria-label={`Show ${label} from ${windowReach(selected)}`}
          className="inline-flex shrink-0 items-center gap-0.5 rounded px-1 py-0.5 font-mono tabular-nums hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
        >
          {selected}
          <ChevronDown aria-hidden className="h-3 w-3" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuRadioGroup value={selected} onValueChange={(choice) => onChoose(choice === spec.default ? undefined : choice)}>
            {windowChoices(spec.default).map((choice) => (
              <DropdownMenuRadioItem key={choice} value={choice} className="text-xs">
                <span className="font-mono">{choice}</span>
                {choice === spec.default && <span className="ml-1.5 text-muted-foreground">(default)</span>}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </footer>
  );
}

function Column({
  def,
  label,
  index,
  items,
  arrived,
  at,
  now,
  window,
  onOpen,
}: {
  def: PipelineDefinition;
  label: string;
  index?: number;
  items: Placed[];
  arrived: Set<string>;
  at: Date | null;
  now: Date;
  window?: WindowChoice;
  onOpen: (docId: string) => void;
}) {
  const noun = `${def.item_label ?? "item"}${items.length === 1 ? "" : "s"}`;
  const hidden = window?.spec.hidden ?? 0;
  return (
    <div className="flex h-[var(--pipeline-column-height)] flex-col rounded-lg border bg-muted/30 max-sm:h-[min(var(--pipeline-column-height),70dvh)]">
      <header className="flex items-center justify-between border-b px-2.5 py-2">
        <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          {index !== undefined && (
            <span className="mr-1.5 font-mono text-[10px] text-muted-foreground/60">{index + 1}</span>
          )}
          {label}
        </span>
        <span
          className={cn(
            "rounded-full px-1.5 font-mono text-[10px] tabular-nums",
            items.length ? "bg-primary/15 text-primary" : "text-muted-foreground/50",
          )}
        >
          {items.length}
          <span className="sr-only"> {noun}</span>
        </span>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        {items.length > 0 ? (
          <ol
            className="flex flex-col gap-2"
            aria-label={`${label}, ${items.length} ${noun}${hidden > 0 ? `, ${hidden} older hidden` : ""}`}
          >
            {items.map((p) => (
              <li key={p.id}>
                <Token
                  def={def}
                  id={p.id}
                  docId={p.docId}
                  onOpen={onOpen}
                  body={p.body}
                  position={p.position}
                  enteredAt={p.enteredAt}
                  activity={p.activity}
                  annotation={p.annotation}
                  arrived={arrived.has(p.id)}
                  at={at}
                  now={now}
                />
              </li>
            ))}
          </ol>
        ) : (
          <p className="flex h-full items-center justify-center text-xs text-muted-foreground">{emptyText(window?.spec)}</p>
        )}
      </div>
      {window && <WindowFooter label={label} {...window} />}
    </div>
  );
}

/** The viewer's own number of cards a column shows before it scrolls; the definition's number clears the choice. */
function CardsPerColumn({ defined, cards, onCards }: { defined: number; cards: number; onCards: (cards: number | undefined) => void }) {
  return (
    <div className="flex items-center justify-end gap-2 text-xs text-muted-foreground">
      <span>Cards per column</span>
      <div className="flex items-center gap-1" role="group" aria-label="cards per column">
        {cardChoices(defined).map((choice) => (
          <button
            key={choice}
            type="button"
            aria-pressed={cards === choice}
            onClick={() => onCards(choice === defined ? undefined : choice)}
            className={cn(
              "rounded px-1.5 py-0.5 font-mono tabular-nums",
              cards === choice ? "bg-primary/15 text-primary" : "text-muted-foreground hover:text-foreground",
            )}
          >
            {choice}
          </button>
        ))}
      </div>
    </div>
  );
}

export function Board({
  def,
  items,
  at,
  cards,
  windows,
  selected,
  busy,
  onCards,
  onWindow,
  onOpen,
}: {
  def: PipelineDefinition;
  items: Item[];
  at: Date | null;
  /** Cards a column shows before it scrolls. */
  cards: number;
  /** The windows the hub applied, by column; empty in replay, which shows no footers. */
  windows: Record<string, ColumnWindow>;
  /** The viewer's choice of window, by column. */
  selected: Record<string, string>;
  /** A new choice of windows is loading. */
  busy: boolean;
  onCards: (cards: number | undefined) => void;
  onWindow: (column: string, window: string | undefined) => void;
  onOpen: (docId: string) => void;
}) {
  const now = useNow(at === null);
  const placed = useMemo(() => place(def, items, at, now), [def, items, at, now]);
  const arrived = useArrivals(placed);
  const byColumn = useMemo(() => {
    const map = new Map<string, Placed[]>();
    for (const p of placed) {
      const list = map.get(p.position.column) ?? [];
      list.push(p);
      map.set(p.position.column, list);
    }
    return map;
  }, [placed]);
  const windowOf = (column: string): WindowChoice | undefined => {
    const spec = windows[column];
    if (!spec) return undefined;
    const chosen = selected[column] ?? spec.default;
    return { spec, selected: chosen, busy: busy && chosen !== spec.window, onChoose: (window) => onWindow(column, window) };
  };
  const exits = (def.exits ?? []).filter((e) => (byColumn.get(e.id)?.length ?? 0) > 0 || (windows[e.id]?.hidden ?? 0) > 0);
  const known = new Set([...def.lanes.map((l) => l.id), ...(def.exits ?? []).map((e) => e.id)]);
  const strays = [...byColumn.keys()].filter((c) => !known.has(c));
  const laneCount = def.lanes.length;
  const height = columnHeight(cards, (def.rungs?.length ?? 0) > 0, hasWindows(def));

  return (
    <div className="min-w-0 space-y-4" style={{ "--pipeline-column-height": `${height}rem` } as CSSProperties}>
      <div className="space-y-2">
        <CardsPerColumn defined={cardsPerColumn(def)} cards={cards} onCards={onCards} />
        <div className="overflow-x-auto pb-2">
          <ol
            className="grid gap-2"
            style={{ gridTemplateColumns: `repeat(${laneCount}, minmax(130px, 1fr))`, minWidth: `${laneCount * 130}px` }}
            aria-label="stages"
          >
            {def.lanes.map((lane, i) => (
              <li key={lane.id}>
                <Column
                  def={def}
                  label={lane.label}
                  index={i}
                  items={byColumn.get(lane.id) ?? []}
                  arrived={arrived}
                  at={at}
                  now={now}
                  window={windowOf(lane.id)}
                  onOpen={onOpen}
                />
              </li>
            ))}
          </ol>
        </div>
      </div>

      {(exits.length > 0 || strays.length > 0) && (
        <section aria-label="items that left the flow">
          <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Left the flow</h3>
          <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-2">
            {exits.map((exit) => (
              <Column
                key={exit.id}
                def={def}
                label={exit.label}
                items={byColumn.get(exit.id) ?? []}
                arrived={arrived}
                at={at}
                now={now}
                window={windowOf(exit.id)}
                onOpen={onOpen}
              />
            ))}
            {strays.map((c) => (
              <Column key={c} def={def} label={`${c} (not in the definition)`} items={byColumn.get(c) ?? []} arrived={arrived} at={at} now={now} onOpen={onOpen} />
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
