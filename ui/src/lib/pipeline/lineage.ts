/**
 * An item's lineage record as a pipeline writes it — one document per item
 * in the pipeline's collection: the current stage, a block per stage with
 * what that stage last did, and a history of events — and the pure
 * functions that read it against a pipeline's definition. Nothing here
 * touches the network, so the live board and the replay agree.
 */

import {
  type PipelineDefinition,
  type PlacingRule,
  type Position,
  type Rule,
  type Tone,
  isAnnotation,
  places,
  positionFor,
  positionOf,
  ruleFor,
  ruleLabel,
} from "./definition";

export interface LineageEvent {
  at: string;
  stage: string;
  event: string;
  run?: string | null;
  detail?: Record<string, unknown> | null;
}

export interface StageBlock {
  last_event?: string;
  at?: string;
  run?: string;
  [key: string]: unknown;
}

export interface LineageBody {
  /** The item's id as the pipeline names it (a ticket key, an order number). */
  ticket?: string;
  item?: string;
  stage?: string;
  created_at?: string;
  updated_at?: string;
  stages?: Record<string, StageBlock>;
  history?: LineageEvent[];
}

/** The item's display id: whichever of the two names the record carries, else the document id. */
export function itemId(body: LineageBody, docId: string): string {
  return body.item || body.ticket || docId;
}

/**
 * Text as the hub's tools hand it to a skill, with the untrusted-content
 * envelope the platform wraps external strings in stripped off.
 */
export function plainText(s: string): string {
  const kept: string[] = [];
  let skipNotice = false;
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (t.startsWith("<<<EXTERNAL_UNTRUSTED_CONTENT") || t.startsWith("<<</EXTERNAL_UNTRUSTED_CONTENT")) {
      skipNotice = t.startsWith("<<<EXTERNAL_UNTRUSTED_CONTENT");
      continue;
    }
    if (skipNotice && t.startsWith("[External content from")) {
      skipNotice = false;
      continue;
    }
    skipNotice = false;
    kept.push(line);
  }
  return kept.join("\n").trim();
}

function pathValue(body: LineageBody, path: string): unknown {
  return path.split(".").reduce<unknown>((v, seg) => (v && typeof v === "object" ? (v as Record<string, unknown>)[seg] : undefined), body);
}

/** The history as it stood at time `t` (null = now): every event before the first one recorded after it. */
function historyAt(body: LineageBody, t: Date | null): LineageEvent[] {
  const history = body.history ?? [];
  const cut = t ? history.findIndex((event) => new Date(event.at) > t) : -1;
  return cut < 0 ? history : history.slice(0, cut);
}

export interface Placement {
  event: LineageEvent;
  rule: PlacingRule;
}

/** The latest event at time `t` (null = now) that the definition places the item with; annotations never place. */
export function lastPlacement(def: PipelineDefinition, body: LineageBody, t: Date | null): Placement | null {
  return historyAt(body, t).reduce<Placement | null>((last, event) => {
    const rule = ruleFor(def, event.stage, event.event, event.detail);
    return rule && places(rule) ? { event, rule } : last;
  }, null);
}

/** What an annotation event notes on an item. */
export interface Annotation {
  tone: Tone;
  label: string;
}

/** An annotation still showing on an item, and when it was recorded. */
export interface StandingAnnotation extends Annotation {
  at: string;
}

function annotationOf(def: PipelineDefinition, rule: Rule, event: LineageEvent): Annotation {
  return { tone: rule.tone ?? "neutral", label: ruleLabel(def, rule, event.detail, event.stage, event.event) };
}

/**
 * The annotation showing on an item at time `t` (null = now): the newest
 * annotation event since the item last moved. A placement clears it; an event
 * the definition has no rule for leaves it standing.
 */
export function standingAnnotation(def: PipelineDefinition, body: LineageBody, t: Date | null): StandingAnnotation | null {
  return historyAt(body, t).reduce<StandingAnnotation | null>((standing, event) => {
    const rule = ruleFor(def, event.stage, event.event, event.detail);
    if (rule && isAnnotation(rule)) return { ...annotationOf(def, rule, event), at: event.at };
    return rule && places(rule) ? null : standing;
  }, null);
}

/**
 * The item's place at time `t` (null = now): its history folded up to that
 * moment. `null` when it had not entered the pipeline yet (or, with no
 * history at all, when the stage block says nothing the definition knows).
 */
export function positionAt(def: PipelineDefinition, body: LineageBody, t: Date | null): Position | null {
  const last = lastPlacement(def, body, t);
  if (last) return positionOf(def, last.rule, last.event.detail);
  if (t) return null;
  const stage = body.stage;
  const block = stage ? body.stages?.[stage] : undefined;
  if (stage && block?.last_event) {
    return positionFor(def, stage, block.last_event, block as Record<string, unknown>);
  }
  return null;
}

/** The rungs that have passed so far, per the definition's rules. */
export function passedRungs(def: PipelineDefinition, body: LineageBody, t: Date | null = null): Set<string> {
  const passed = new Set<string>();
  for (const event of historyAt(body, t)) {
    const rule = ruleFor(def, event.stage, event.event, event.detail);
    if (!rule || !places(rule) || !rule.rung_passed || !rule.rung_field) continue;
    const rung = event.detail?.[rule.rung_field];
    if (typeof rung === "string" && rung) passed.add(rung);
  }
  return passed;
}

/** The first link a stage block carries, per the definition's link fields. */
export function itemLink(def: PipelineDefinition, body: LineageBody): { url: string; label: string } | undefined {
  for (const link of def.link_fields ?? []) {
    for (const block of Object.values(body.stages ?? {})) {
      const url = block[link.field];
      if (typeof url === "string" && /^https?:\/\//.test(url)) {
        const m = url.match(/\/(?:pull|issues|-\/merge_requests)\/(\d+)/);
        return { url, label: m ? `#${m[1]}` : link.label };
      }
    }
  }
  return undefined;
}

export function summary(def: PipelineDefinition, body: LineageBody): string {
  if (!def.summary_field) return "";
  const s = pathValue(body, def.summary_field);
  return typeof s === "string" ? plainText(s) : "";
}

/** The run (workflow instance) that last touched the item. */
export function lastRun(body: LineageBody): string | undefined {
  const stage = body.stage;
  const run = stage ? body.stages?.[stage]?.run : undefined;
  return typeof run === "string" ? run : undefined;
}

/** When the item entered its current column (its last moving event). */
export function enteredAt(def: PipelineDefinition, body: LineageBody, t: Date | null = null): string | undefined {
  return lastPlacement(def, body, t)?.event.at ?? body.updated_at;
}

export interface FeedEvent extends LineageEvent {
  item: string;
  /** Where a placing event puts the item. */
  position: Position | null;
  /** What an annotation event notes on it. */
  annotation: Annotation | null;
}

/** Every event across the records, newest first. */
export function allEvents(def: PipelineDefinition, items: { id: string; body: LineageBody }[]): FeedEvent[] {
  const events: FeedEvent[] = [];
  for (const { id, body } of items) {
    for (const event of body.history ?? []) {
      const rule = ruleFor(def, event.stage, event.event, event.detail);
      events.push({
        ...event,
        item: itemId(body, id),
        position: rule && places(rule) ? positionOf(def, rule, event.detail) : null,
        annotation: rule && isAnnotation(rule) ? annotationOf(def, rule, event) : null,
      });
    }
  }
  return events.sort((a, b) => b.at.localeCompare(a.at));
}

function str(detail: Record<string, unknown> | null | undefined, key: string): string {
  const v = detail?.[key];
  return typeof v === "string" ? v : "";
}

/** A one-line reading of an event for the feed. */
export function describe(event: FeedEvent): string {
  const d = event.detail ?? {};
  const bits: string[] = [];
  const verdict = str(d, "verdict") || str(d, "label") || str(d, "outcome") || str(d, "state");
  if (verdict) bits.push(verdict);
  const rung = str(d, "rung");
  if (rung) bits.push(`rung ${rung}`);
  const text = str(d, "detail") || str(d, "summary") || str(d, "reason");
  if (text) bits.push(plainText(text));
  return bits.join(" · ");
}
