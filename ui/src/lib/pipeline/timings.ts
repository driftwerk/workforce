/**
 * Pure readings of an item's timings for the item panel: the figures as of
 * now while the clock runs, the words for each stretch and milestone, and
 * the notes that say what the figures leave out.
 */

import { apiErrorMessage } from "@/lib/api-error";
import type { ColumnTimes, ItemTimings, TimedSpan, Visit, VisitKind } from "@/lib/api/pipelines";
import { formatDateTime, formatDuration } from "@/lib/utils";
import { type CoverageNote, count } from "./costs";

/** A duration as the panel shows it: a dash when unknown, "0s" when none. */
export function durationText(ms: number | null): string {
  if (ms === null) return "—";
  return ms === 0 ? "0s" : formatDuration(ms);
}

function grown<T extends TimedSpan>(span: T, ms: number, working: boolean): T {
  return {
    ...span,
    elapsed_ms: span.elapsed_ms + ms,
    work_ms: working && span.work_ms !== null ? span.work_ms + ms : span.work_ms,
    waiting_ms: !working && span.waiting_ms !== null ? span.waiting_ms + ms : span.waiting_ms,
  };
}

/**
 * The timings as of `now`: while the clock runs, the time since the hub read
 * them is added to the totals, the running stretch and its column, as work
 * while a run is working the item and as waiting otherwise.
 */
export function live(t: ItemTimings, now: Date): ItemTimings {
  const ms = now.getTime() - new Date(t.as_of).getTime();
  if (!t.total.open || !(ms > 0)) return t;
  const { working } = t.total;
  const running = t.visits.find((visit) => visit.left_at === null);
  return {
    ...t,
    total: { ...grown(t.total, ms, working), in_pipeline_ms: t.total.in_pipeline_ms + ms },
    visits: t.visits.map((visit) => (visit === running ? grown(visit, ms, working) : visit)),
    columns: t.columns.map((column) =>
      running?.kind === "column" && column.column === running.column ? grown(column, ms, working) : column,
    ),
  };
}

const LEADING: Record<Exclude<VisitKind, "column">, string> = {
  before_pipeline: "Before the pipeline",
  trimmed: "Earlier history (trimmed)",
  unplaced: "Not yet on the board",
};

export function visitLabel(visit: Visit): string {
  return visit.kind === "column" ? (visit.label ?? visit.column ?? "") : LEADING[visit.kind];
}

/** What the clock starts from: the definition's name for a recorded origin, else the first record. */
export function originText(t: ItemTimings): string {
  return t.origin.recorded && t.origin.label ? t.origin.label : "First recorded";
}

export interface MilestoneReading {
  label: string;
  value: string;
  hint: string | null;
}

/** Each milestone as a stat: how long it took from the origin, or that it is not reached yet (or never will be). */
export function milestoneReadings(t: ItemTimings): MilestoneReading[] {
  const from = originText(t);
  return t.milestones.map((milestone) => ({
    label: `${from} → ${milestone.label}`,
    value:
      milestone.from_origin_ms !== null ? durationText(milestone.from_origin_ms) : t.total.open ? "Not yet" : "Never",
    hint: milestone.reached_at
      ? formatDateTime(milestone.reached_at)
      : t.total.open
        ? `${durationText(t.total.elapsed_ms)} so far`
        : null,
  }));
}

/** The share of a column's time spent working; null when work is unknown or no time passed. */
export function workShare(column: ColumnTimes): number | null {
  return column.work_ms === null || column.elapsed_ms === 0 ? null : column.work_ms / column.elapsed_ms;
}

/** One note for each thing the figures do not cover, the warning first. */
export function timingNotes({ coverage: c, origin }: ItemTimings, itemLabel: string): CoverageNote[] {
  const moment = (origin.label ?? "start").toLowerCase();
  return [
    c.runs_unmatched > 0 && {
      text: `${count(c.runs_unmatched, "run", "runs")} this ${itemLabel} names could not be matched to a workflow run this pipeline counts, so ${
        c.runs_unmatched === 1 ? "its" : "their"
      } time shows as waiting`,
      warn: true,
    },
    c.work_unavailable && {
      text: "Work and waiting are not shown: telemetry is not configured on this hub",
      warn: false,
    },
    !c.origin_recorded &&
      origin.label !== null && {
        text: `No ${moment} time is recorded for this ${itemLabel}, so times run from when the pipeline first recorded it`,
        warn: false,
      },
    c.origin_after_start && {
      text: `The recorded ${moment} time is later than when the pipeline first recorded this ${itemLabel}, so times run from the first record`,
      warn: false,
    },
    c.runs_by_lifetime > 0 && {
      text: `${count(c.runs_by_lifetime, "run has", "runs have")} no step history, so ${
        c.runs_by_lifetime === 1 ? "its" : "their"
      } whole run counts as work`,
      warn: false,
    },
    c.history_truncated && { text: "Older history was trimmed; its time is shown as one stretch", warn: false },
    c.clock_adjusted && {
      text: "Some times were adjusted: events recorded out of order, and run time before the clock started or after it stopped, are not counted",
      warn: false,
    },
  ].filter((note): note is CoverageNote => note !== false);
}

/** Why the timings could not be shown, from the error the request failed with. */
export function timingsErrorText(error: unknown): string {
  const status = (error as { status?: number } | null)?.status;
  if (status === 404) return "Timings are unavailable: this item is no longer in the pipeline";
  if (status === 403) return "Timings are not visible to this login";
  return `Timings could not be loaded: ${apiErrorMessage(error)}`;
}
