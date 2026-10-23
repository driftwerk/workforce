import { gateway } from "../api-client";

// ── Domain types ─────────────────────────────────────────────────────────────

/** The three learning kinds the browser dropdown filters on. The corpus may
 *  also carry `null` (no `learning:` tag) or an `"unknown"` bucket, so display
 *  code must tolerate values outside this set. */
export type LearningKind = "tool_failure" | "user_correction" | "repeated_retry";

export const LEARNING_KINDS: LearningKind[] = [
  "tool_failure",
  "user_correction",
  "repeated_retry",
];

export interface Lesson {
  id: string;
  owner: string;
  content: string;
  importance: number;
  confidence: number;
  applied: number;
  /** May be null when the lesson carries no `learning:` tag. */
  learning_kind: string | null;
  date: string;
  trigger_tags: string[];
}

export interface LessonsListResponse {
  lessons: Lesson[];
  total: number;
}

export interface LessonsSummary {
  total: number;
  applied_at_least_once: number;
  never_applied: number;
  avg_confidence: number;
  open_recurrences: number;
  extraction_cost_usd: number;
  window_days: number;
}

export interface CorpusGrowthPoint {
  day: string;
  cumulative: number;
}

export interface TimeseriesPoint {
  day: string;
  extracted: number;
  injected: number;
  suppressed: number;
  reinforced_positive: number;
  reinforced_negative: number;
  merged_recurrence: number;
  hit: number;
  recall_miss: number;
  suppressed_repeat: number;
  superseded: number;
  pruned: number;
}

/** Prevention is null as a whole object when there is no telemetry store; when
 *  present its `rate` is still null until at least one injection is scored. */
export interface Prevention {
  prevented: number;
  failed: number;
  rate: number | null;
}

/** Recall hit-rate: of re-taught lessons, how often the lesson was actually
 *  injected this turn (`hits`) vs missed by retrieval (`misses`). Null as a
 *  whole when there is no telemetry store; `rate` null until the first event. */
export interface Recall {
  hits: number;
  misses: number;
  rate: number | null;
}

export interface LeaderboardEntry {
  id: string;
  content: string;
  applied: number;
  confidence: number;
}

export interface LessonsMetrics {
  summary: LessonsSummary;
  by_kind: Record<string, number>;
  corpus_growth: CorpusGrowthPoint[];
  timeseries: TimeseriesPoint[];
  prevention: Prevention | null;
  recall: Recall | null;
  leaderboard: LeaderboardEntry[];
}

export interface RecurrenceLesson {
  id: string;
  content: string;
  date: string;
  applied: number;
}

export interface RecurrenceCluster {
  trigger: string;
  dates: string[];
  lessons: RecurrenceLesson[];
}

export interface RecurrencesResponse {
  clusters: RecurrenceCluster[];
}

export interface DeleteLessonResponse {
  deleted: boolean;
  id: string;
}

// ── Background jobs (cleanup / backfill) ─────────────────────────────────────

export interface JobKickResponse {
  job_id: string;
  status: string;
}

export interface CleanupReport {
  lessons_scanned: number;
  pruned: number;
  merged: number;
  kept: number;
  errors: number;
}

export interface BackfillReport {
  agents_scanned: number;
  sessions_scanned: number;
  turns_scanned: number;
  signals_detected: number;
  lessons_written: number;
}

/** Job status. `report` is present only on `done`, `error` only on `failed`,
 *  and the report shape is discriminated by `kind`. */
export type JobStatus =
  | {
      status: "running";
      kind: "cleanup" | "backfill";
      dry_run: boolean;
    }
  | {
      status: "done";
      kind: "cleanup";
      dry_run: boolean;
      report: CleanupReport;
    }
  | {
      status: "done";
      kind: "backfill";
      dry_run: boolean;
      report: BackfillReport;
    }
  | {
      status: "failed";
      kind: "cleanup" | "backfill";
      error: string;
    };

// ── Request params ───────────────────────────────────────────────────────────

export interface LessonsFilter {
  agent?: string;
  kind?: string;
  /** "0" → never applied, "1" → applied at least once. Omit for all. */
  applied?: "0" | "1";
  q?: string;
  limit?: number;
  offset?: number;
}

export interface CleanupBody {
  agent?: string;
  dry_run?: boolean;
  merge_similarity?: number;
}

export interface BackfillBody {
  agent?: string;
  dry_run?: boolean;
  since_days?: number;
}

function buildSearch(params: object): string {
  const search = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") search.set(k, String(v));
  }
  const qs = search.toString();
  return qs ? `?${qs}` : "";
}

// ── API client (bearer carries admin scope — no ?token= needed) ──────────────

export const lessonsApi = {
  list: (filter: LessonsFilter = {}) =>
    gateway.get<LessonsListResponse>(`/api/lessons${buildSearch(filter)}`),

  metrics: (agent: string, days: number) =>
    gateway.get<LessonsMetrics>(
      `/api/lessons/metrics${buildSearch({ agent, days })}`,
    ),

  recurrences: (agent: string) =>
    gateway.get<RecurrencesResponse>(
      `/api/lessons/recurrences${buildSearch({ agent })}`,
    ),

  remove: (id: string) =>
    gateway.del<DeleteLessonResponse>(
      `/api/lessons/${encodeURIComponent(id)}`,
    ),

  cleanup: (body: CleanupBody) =>
    gateway.post<JobKickResponse>("/api/lessons/cleanup", body),

  backfill: (body: BackfillBody) =>
    gateway.post<JobKickResponse>("/api/lessons/backfill", body),

  jobStatus: (id: string) =>
    gateway.get<JobStatus>(`/api/lessons/jobs/${encodeURIComponent(id)}`),
};
