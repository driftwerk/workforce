import { gateway } from "../api-client";
import type { Column, PipelineDefinition, Tone } from "@/lib/pipeline/definition";
import type { LineageBody } from "@/lib/pipeline/lineage";

export interface PipelineCounts {
  items: number;
  columns: Record<string, number>;
  unplaced: number;
  last_moved_at?: string | null;
}

/** One row of the catalogue. */
export interface PipelineSummary {
  scope: string;
  pipeline: string;
  name: string;
  description?: string | null;
  item_label: string;
  collection: string;
  lanes: Column[];
  exits: Column[];
  sources: string[];
  version: number;
  counts: PipelineCounts;
}

/** Something the pipeline wants every viewer of its board to know now. */
export interface PipelineNotice {
  key: string;
  tone: Tone;
  title: string;
  detail: string | null;
  link: string | null;
  raised_at: string;
  expires_at: string | null;
}

export interface PipelineDetail extends PipelineSummary {
  definition: PipelineDefinition;
  /** Whether this login may read what the pipeline's items cost. */
  can_view_costs: boolean;
  /** The notices raised and not yet cleared or expired. */
  notices: PipelineNotice[];
}

/** `billed` is charged per token; `estimated` is subscription use priced at list rates. */
export type Billing = "billed" | "estimated";

export interface ModelCosts {
  engine: string | null;
  model: string | null;
  billing: Billing;
  attempts: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

export interface AttemptCosts {
  engine: string;
  model: string | null;
  billing: Billing;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  /** Why the chain moved on from the attempt; `null` on records that predate it. */
  error: string | null;
}

export interface StepCosts {
  step_id: string;
  failed: boolean;
  engine: string | null;
  model: string | null;
  billing: Billing | null;
  rounds: number;
  input_tokens: number | null;
  output_tokens: number | null;
  /** `null` when the step ran before its cost was recorded. */
  cost_usd: number | null;
  handed_over: AttemptCosts[];
  started_at: string;
  completed_at: string;
  budget_capped: boolean;
}

export interface RunCosts {
  run: string;
  workflow: string;
  status: string;
  stage: string;
  label: string;
  started_at: string;
  updated_at: string;
  /** The item the run is counted on. */
  booked_to: string;
  also_on: string[];
  billed_usd: number;
  estimated_usd: number;
  steps: StepCosts[];
}

export interface StageCosts {
  stage: string;
  label: string;
  workflows: string[];
  runs: number;
  billed_usd: number;
  estimated_usd: number;
  input_tokens: number;
  output_tokens: number;
  models: ModelCosts[];
}

export interface Coverage {
  runs_named: number;
  runs_costed: number;
  runs_unmatched: number;
  runs_shared: number;
  steps_unrecorded: number;
  history_truncated: boolean;
}

export interface ItemCosts {
  pipeline: string;
  scope: string;
  item: string;
  currency: "USD";
  as_of: string;
  total: {
    billed_usd: number;
    estimated_usd: number;
    input_tokens: number;
    output_tokens: number;
    runs: number;
  };
  coverage: Coverage;
  stages: StageCosts[];
  runs: RunCosts[];
}

export type VisitKind = "before_pipeline" | "trimmed" | "unplaced" | "column";

/** Durations in milliseconds; work and waiting are `null` when the hub keeps no workflow runs. */
export interface TimedSpan {
  elapsed_ms: number;
  work_ms: number | null;
  waiting_ms: number | null;
}

/** One stretch of an item's time: in a column, or before it had one. */
export interface Visit extends TimedSpan {
  kind: VisitKind;
  column: string | null;
  label: string | null;
  entered_at: string;
  /** `null` for the stretch still running. */
  left_at: string | null;
}

export interface ColumnTimes extends TimedSpan {
  column: string;
  label: string;
  visits: number;
}

export interface Milestone {
  column: string;
  label: string;
  reached_at: string | null;
  from_origin_ms: number | null;
}

export interface RunTimes {
  run: string;
  workflow: string;
  status: string;
  started_at: string | null;
  ended_at: string | null;
  work_ms: number;
  timed_by: "steps" | "lifetime";
}

export interface TimingCoverage {
  runs_named: number;
  runs_timed: number;
  runs_unmatched: number;
  runs_by_lifetime: number;
  work_unavailable: boolean;
  origin_recorded: boolean;
  origin_after_start: boolean;
  history_truncated: boolean;
  clock_adjusted: boolean;
}

export interface ItemTimings {
  pipeline: string;
  scope: string;
  item: string;
  as_of: string;
  /** Where the clock starts: a recorded moment, else when the pipeline first recorded the item. */
  origin: { at: string; label: string | null; recorded: boolean };
  started_at: string;
  finished_at: string | null;
  total: TimedSpan & {
    in_pipeline_ms: number;
    /** The clock is still running. */
    open: boolean;
    /** A run the item's record names is in flight. */
    working: boolean;
  };
  milestones: Milestone[];
  visits: Visit[];
  columns: ColumnTimes[];
  runs: RunTimes[];
  coverage: TimingCoverage;
}

export interface PipelineItem {
  item: string;
  column: string | null;
  version: number;
  updated_at: string;
  record: LineageBody;
}

/** The window the hub applied to a column, and how many of its items that left out. */
export interface ColumnWindow {
  window: string;
  /** The definition's window for the column. */
  default: string;
  hidden: number;
}

/** What the board shows of a pipeline: every item still in play, and the finished ones within their column's window. */
export interface PipelineBoard {
  pipeline: string;
  scope: string;
  count: number;
  items: PipelineItem[];
  /** Every windowed column, by id. */
  windows: Record<string, ColumnWindow>;
  /** The earliest and latest events across every item, hidden ones included. */
  span: { from: string; to: string } | null;
}

export interface PipelineEvent {
  item: string;
  at: string;
  stage: string;
  event: string;
  run?: string | null;
  detail?: Record<string, unknown> | string | null;
  column?: string | null;
  tone?: Tone | null;
  label?: string | null;
  /** The event annotates the item rather than moving it. */
  annotation: boolean;
}

const enc = encodeURIComponent;

export const pipelinesApi = {
  catalogue: () => gateway.get<{ count: number; pipelines: PipelineSummary[] }>("/api/pipelines"),
  get: (scope: string, id: string) => gateway.get<PipelineDetail>(`/api/pipelines/${enc(scope)}/${enc(id)}`),
  board: (scope: string, id: string, windows: string | undefined) =>
    gateway.get<PipelineBoard>(
      `/api/pipelines/${enc(scope)}/${enc(id)}/items?view=board${windows ? `&windows=${enc(windows)}` : ""}`,
    ),
  events: (scope: string, id: string, limit = 500) =>
    gateway.get<{ count: number; events: PipelineEvent[] }>(
      `/api/pipelines/${enc(scope)}/${enc(id)}/events?limit=${limit}`,
    ),
  itemCosts: (scope: string, id: string, item: string) =>
    gateway.get<ItemCosts>(`/api/pipelines/${enc(scope)}/${enc(id)}/items/${enc(item)}/costs`),
  itemTimings: (scope: string, id: string, item: string) =>
    gateway.get<ItemTimings>(`/api/pipelines/${enc(scope)}/${enc(id)}/items/${enc(item)}/timings`),
};
