/**
 * What is happening to an item at a moment, read from its record through the
 * definition's `claim`, `lease_field` and `due_field` rules: a run working it,
 * a time it is next due, or neither. Evaluated at the board's clock, so the
 * live board and the replay order columns alike.
 */

import { formatDateTime, formatDuration, formatRelativeTime } from "@/lib/utils";
import type { PipelineDefinition, Rule } from "./definition";
import { type LineageBody, type LineageEvent, lastPlacement } from "./lineage";

/** How long a claim holds when its rule names a lease field the event does not carry, as claims recorded before leases were. */
export const LEGACY_LEASE_MINUTES = 45;

export type Activity =
  | { kind: "working"; since: string; until: Date | null }
  | { kind: "scheduled"; due: Date }
  | { kind: "other" };

const OTHER: Activity = { kind: "other" };

function instant(value: unknown): Date | null {
  if (typeof value !== "string") return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

function minutes(value: unknown): number | null {
  const n = typeof value === "number" ? value : typeof value === "string" && value.trim() ? Number(value) : NaN;
  return Number.isFinite(n) ? n : null;
}

/** When a claim's lease runs out; null when its rule names no lease or the claim's time does not parse. */
function leaseEnd(event: LineageEvent, rule: Rule): Date | null {
  const since = instant(event.at);
  if (!rule.lease_field || !since) return null;
  const lease = minutes(event.detail?.[rule.lease_field]) ?? LEGACY_LEASE_MINUTES;
  return new Date(since.getTime() + lease * 60_000);
}

/**
 * The item's activity as its history stood at `at` (null = now), with leases
 * and due times read against `at ?? now`. A claim whose lease has run out is
 * due from when it was claimed: the run that claimed it is gone.
 */
export function activityAt(def: PipelineDefinition, body: LineageBody, at: Date | null, now: Date): Activity {
  const last = lastPlacement(def, body, at);
  if (!last) return OTHER;
  const { event, rule } = last;
  if (rule.claim) {
    const until = leaseEnd(event, rule);
    return until && until <= (at ?? now)
      ? { kind: "scheduled", due: new Date(event.at) }
      : { kind: "working", since: event.at, until };
  }
  const due = rule.due_field ? instant(event.detail?.[rule.due_field]) : null;
  return due ? { kind: "scheduled", due } : OTHER;
}

/** What a column sorts by: the item's display id, when it entered the column, and its activity. */
export interface Ranked {
  id: string;
  enteredAt?: string;
  activity: Activity;
}

const TIER: Record<Activity["kind"], number> = { working: 0, scheduled: 1, other: 2 };

function time(at: string | undefined): number {
  const ms = at ? new Date(at).getTime() : NaN;
  return Number.isNaN(ms) ? -Infinity : ms;
}

function withinTier(a: Ranked, b: Ranked): number {
  if (a.activity.kind === "working" && b.activity.kind === "working") {
    return time(b.activity.since) - time(a.activity.since);
  }
  if (a.activity.kind === "scheduled" && b.activity.kind === "scheduled") {
    return a.activity.due.getTime() - b.activity.due.getTime();
  }
  return time(b.enteredAt) - time(a.enteredAt);
}

/**
 * A column's order: work in progress, newest claim first; then what is due,
 * most overdue first; then the rest, newest arrival first. Ties go by id.
 */
export function byActivity(a: Ranked, b: Ranked): number {
  return (
    TIER[a.activity.kind] - TIER[b.activity.kind] ||
    withinTier(a, b) ||
    (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)
  );
}

/** A card's few words on its activity at `t`; null when it has none to say. */
export function activityCue(activity: Activity, t: Date): string | null {
  switch (activity.kind) {
    case "working":
      return activity.until ? "working now" : `claimed ${formatRelativeTime(activity.since, t)}`;
    case "scheduled": {
      const ms = activity.due.getTime() - t.getTime();
      if (Math.abs(ms) < 60_000) return "due now";
      return ms > 0 ? `due in ${formatDuration(ms)}` : `overdue by ${formatDuration(-ms)}`;
    }
    case "other":
      return null;
  }
}

/** The moment behind a cue, for the card's tooltip. */
export function activityDetail(activity: Activity): string | null {
  switch (activity.kind) {
    case "working":
      return activity.until ? `lease until ${formatDateTime(activity.until.toISOString())}` : null;
    case "scheduled":
      return `next due ${formatDateTime(activity.due.toISOString())}`;
    case "other":
      return null;
  }
}
