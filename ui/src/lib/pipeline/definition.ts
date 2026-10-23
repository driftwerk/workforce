/**
 * A pipeline declares itself: a document in the `pipelines` collection of
 * the scope whose items it tracks (`agent:<id>/pipelines/<name>`), naming
 * the item collection, the lanes an item moves through, the exits it can
 * take, and the rules that put an event on a column. The page renders
 * whatever pipelines a hub has and knows nothing about any one of them.
 */

/**
 * `warn` means a person must act; `healing` means the pipeline is still
 * working on it itself (a retry, a re-check, a send-back) and nothing is
 * needed from anyone yet.
 */
export type Tone = "active" | "good" | "warn" | "healing" | "bad" | "neutral";

export interface Column {
  id: string;
  label: string;
  /** How long a finished item stays on the board after entering this column; absent: always shown. */
  window?: string;
}

/** One way an event can place or annotate an item; the first rule whose `when` holds wins. */
export interface Rule {
  /** Every key must equal the event's `detail[key]` (as a string). */
  when?: Record<string, string>;
  /** A lane id (the item stays in the flow) or an exit id (it parks); absent on an annotation rule. */
  column?: string;
  /** The event notes something on the item without moving it. */
  annotation?: boolean;
  tone?: Tone;
  /** Shown on the token instead of the column's label. */
  label?: string;
  /** The `detail` field whose non-empty text replaces `label`. */
  label_field?: string;
  /** The `detail` field naming the rung this event is on (e.g. `rung`). */
  rung_field?: string;
  /** The rung named by `rung_field` counts as passed after this event. */
  rung_passed?: boolean;
  /** A run is working the item: the claim holds while this is the item's latest placement. */
  claim?: boolean;
  /** The `detail` field holding the claim's lease in minutes; past it the claim no longer holds. */
  lease_field?: string;
  /** The `detail` field holding when the item is next due (RFC 3339). */
  due_field?: string;
}

/** How an item's time is read; each part falls back to the hub's default when absent. */
export interface Timings {
  /** The `detail` field a source records when the item came into being outside the pipeline, and that moment's name. */
  origin?: { field: string; label: string };
  /** Lane ids timed from the origin; the last lane when absent. */
  milestones?: string[];
  /** Lane or exit ids where an item's clock stops; the last lane when absent. */
  ends?: string[];
}

export interface LinkField {
  /** Dotted path in a stage block, e.g. `pr_url`; the first stage that has it wins. */
  field: string;
  label: string;
}

export interface PipelineDefinition {
  name: string;
  description?: string;
  /** How the pipeline works, in markdown, shown on the board above the definition. */
  guide?: string;
  /** What one item is called: "ticket", "order", "candidate". Default "item". */
  item_label?: string;
  /** The collection, in the same scope, holding one document per item. */
  collection: string;
  lanes: Column[];
  exits?: Column[];
  /** The rung ladder shown on tokens whose position names a rung, in order. */
  rungs?: string[];
  timings?: Timings;
  /** How the board lays the pipeline out: `cards` a column shows before it scrolls, 10 when absent. */
  board?: { cards?: number };
  /** `"<stage>/<event>"` (or `"<stage>/*"`) → rules. An event with no rules does not move the item. */
  events: Record<string, Rule[]>;
  /** Dotted path into the item body for its one-line summary, e.g. `stages.implement.summary`. */
  summary_field?: string;
  /** Fields worth a link on the token, e.g. a PR URL. */
  link_fields?: LinkField[];
  /** Which workflows or skills feed this pipeline — for people and the AI. */
  sources?: string[];
  /** The author's own revision counter; a shipped definition redefines only when the stored one is older. */
  revision?: number;
}

/** A definition with where it was found. */
export interface PipelineRef {
  /** The store scope spelling the items and the definition live in, e.g. `agent:example`. */
  scope: string;
  /** The definition's document id. */
  id: string;
  definition: PipelineDefinition;
}

export interface Position {
  column: string;
  /** In a lane (the flow) rather than parked at an exit. */
  flow: boolean;
  tone: Tone;
  label: string;
  rung?: string;
}

const REQUIRED = ["name", "collection", "lanes", "events"] as const;

/** Whether a document body is a usable definition; a broken one is skipped, not rendered. */
export function isDefinition(body: unknown): body is PipelineDefinition {
  if (!body || typeof body !== "object") return false;
  const b = body as Record<string, unknown>;
  if (REQUIRED.some((k) => !(k in b))) return false;
  if (typeof b.name !== "string" || typeof b.collection !== "string") return false;
  if (!Array.isArray(b.lanes) || b.lanes.length === 0) return false;
  return typeof b.events === "object" && b.events !== null;
}

function detailString(detail: Record<string, unknown> | null | undefined, key: string): string | undefined {
  const v = detail?.[key];
  if (v === undefined || v === null) return undefined;
  return typeof v === "string" ? v : String(v);
}

function matches(rule: Rule, detail: Record<string, unknown> | null | undefined): boolean {
  if (!rule.when) return true;
  return Object.entries(rule.when).every(([k, v]) => detailString(detail, k) === v);
}

/** The rule an event resolves to, or null when the definition says nothing about it. */
export function ruleFor(
  def: PipelineDefinition,
  stage: string,
  event: string,
  detail: Record<string, unknown> | null | undefined,
): Rule | null {
  const rules = def.events[`${stage}/${event}`] ?? def.events[`${stage}/*`];
  if (!rules) return null;
  return rules.find((r) => matches(r, detail)) ?? null;
}

/** The longest label a `label_field` yields, in code points. */
export const LABEL_MAX = 120;

const BLANK = /[\p{White_Space}\p{Cc}]/u;

/**
 * A detail value as label text: one line, without `<` or `>` (so no Slack
 * mention or link markup survives), at most `LABEL_MAX` code points. Null
 * when the value is not text or holds nothing to show.
 */
export function labelText(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const line = Array.from(value, (c) => (c === "<" || c === ">" ? "" : BLANK.test(c) ? " " : c))
    .join("")
    .split(" ")
    .filter(Boolean)
    .join(" ");
  const points = Array.from(line);
  if (points.length === 0) return null;
  return points.length <= LABEL_MAX ? line : `${points.slice(0, LABEL_MAX - 1).join("").replace(/ +$/, "")}…`;
}

export function isAnnotation(rule: Rule): boolean {
  return rule.annotation === true;
}

/** A rule that puts an item on a column. */
export type PlacingRule = Rule & { column: string };

export function places(rule: Rule): rule is PlacingRule {
  return !isAnnotation(rule) && rule.column !== undefined;
}

function fieldLabel(rule: Rule, detail: Record<string, unknown> | null | undefined): string | null {
  return rule.label_field ? labelText(detail?.[rule.label_field]) : null;
}

/** What an event under `rule` is called on the board: its `label_field` text, its label, its column's label, else the event itself. */
export function ruleLabel(
  def: PipelineDefinition,
  rule: Rule,
  detail: Record<string, unknown> | null | undefined,
  stage: string,
  event: string,
): string {
  return (
    fieldLabel(rule, detail) ?? rule.label ?? (rule.column === undefined ? `${stage}/${event}` : columnLabel(def, rule.column))
  );
}

export function columnLabel(def: PipelineDefinition, column: string): string {
  return (
    def.lanes.find((l) => l.id === column)?.label ??
    def.exits?.find((e) => e.id === column)?.label ??
    column
  );
}

export function isLane(def: PipelineDefinition, column: string): boolean {
  return def.lanes.some((l) => l.id === column);
}

/** The place a rule puts an item, reading the rung from the event's detail. */
export function positionOf(def: PipelineDefinition, rule: PlacingRule, detail: Record<string, unknown> | null | undefined): Position {
  return {
    column: rule.column,
    flow: isLane(def, rule.column),
    tone: rule.tone ?? "neutral",
    label: fieldLabel(rule, detail) ?? rule.label ?? columnLabel(def, rule.column),
    rung: rule.rung_field ? detailString(detail, rule.rung_field) : undefined,
  };
}

/** The place one event puts an item; null when it does not move it. */
export function positionFor(
  def: PipelineDefinition,
  stage: string,
  event: string,
  detail: Record<string, unknown> | null | undefined,
): Position | null {
  const rule = ruleFor(def, stage, event, detail);
  return rule && places(rule) ? positionOf(def, rule, detail) : null;
}

/** The lanes an item's time is reported against, in lane order. */
export function milestoneLanes(def: PipelineDefinition): Column[] {
  const ids = def.timings?.milestones;
  return ids ? def.lanes.filter((lane) => ids.includes(lane.id)) : def.lanes.slice(-1);
}

/** The lane and exit ids where an item's clock stops. */
export function clockEnds(def: PipelineDefinition): string[] {
  return def.timings?.ends ?? def.lanes.slice(-1).map((lane) => lane.id);
}
