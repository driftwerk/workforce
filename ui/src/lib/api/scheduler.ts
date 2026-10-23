import { gateway } from "../api-client";
import type { PrincipalKind } from "./cost";

// ── Types ─────────────────────────────────────────────────────────────────

export type Visibility = "private" | "global";

/**
 * Which session a job's runs use: a new one every run, or one kept across
 * runs, which every run replays in full.
 */
export type SessionMode = "fresh" | "pinned";

export interface ScheduledJob {
  id: string;
  name: string;
  description: string | null;
  message: string | null;
  enabled: boolean;
  schedule: JobSchedule;
  next_run: string | null;
  last_run: string | null;
  run_count: number;
  created_at: string;
  is_reminder: boolean;
  owner: string | null;
  visibility: Visibility;
  /** Null for a job that runs no agent turn (a reminder, a workflow start). */
  session: SessionMode | null;
  /** What the job's runs have cost so far; only listings carry it. */
  spent_usd?: number | null;
  owner_display_name: string | null;
  owner_kind: PrincipalKind | null;
  agent_id?: string;
}

export type JobSchedule =
  | { kind: "at"; at: string }
  | { kind: "every"; every_secs: number; anchor?: string }
  | { kind: "cron"; expr: string; tz?: string };

export interface CreateJobRequest {
  name: string;
  description?: string;
  message: string;
  schedule: JobSchedule;
  channel_id?: string;
  thread_id?: string;
  model?: string;
  visibility?: Visibility;
  /**
   * When true, the agent posts its own result during the run (e.g. via
   * messaging.send) and the scheduler does NOT also deliver the agent's reply —
   * preventing a duplicate "report + chatty Done" double-post on report-style
   * jobs. Default false: the scheduler delivers the reply.
   */
  agent_owns_delivery?: boolean;
}

export interface UpdateJobRequest {
  name?: string;
  schedule?: JobSchedule;
  enabled?: boolean;
  visibility?: Visibility;
  session?: SessionMode;
  /** The user has seen what the edit is forecast to cost and approved it. */
  confirmed?: boolean;
}

/**
 * What `PUT /api/schedules/:id` answers (409) for an edit that makes a job
 * cost more: re-send it with `confirmed` once the user approves.
 */
export interface CostConfirmation {
  status: "needs_confirmation";
  reason: string;
  estimate: {
    per_run_usd: number;
    runs_per_day: number;
    per_day_usd: number;
    per_month_usd: number;
  };
}

/** What a refused update says: the cost to approve, or why it cannot apply. */
export function updateRefusal(
  error: unknown,
): { confirmation: CostConfirmation } | { message: string } {
  const text = error instanceof Error ? error.message : String(error);
  try {
    const body = JSON.parse(text);
    if (body?.status === "needs_confirmation") return { confirmation: body };
    if (typeof body?.error === "string") return { message: body.error };
  } catch {
    // Not a JSON body: the text is the message.
  }
  return { message: text };
}

export interface JobRunRecord {
  job_id: string;
  job_name: string;
  started_at: string;
  finished_at: string;
  duration_ms: number;
  success: boolean;
  output: string;
  delivered: boolean;
  /** What the run did other than the job as stored asks. */
  note?: string | null;
}

// ── API Client ────────────────────────────────────────────────────────────

export const schedulerApi = {
  /** GET /api/schedules — jobs visible to the caller (own + public; all for admins). */
  list: () => gateway.get<{ jobs: ScheduledJob[] }>("/api/schedules"),

  /** GET /api/schedules/:id — get a single job the caller may see. */
  get: (id: string) =>
    gateway.get<ScheduledJob>(`/api/schedules/${encodeURIComponent(id)}`),

  /** POST /api/schedules — create a job owned by the caller. */
  create: (data: CreateJobRequest) =>
    gateway.post<{ created: boolean; id: string; job: ScheduledJob }>(
      "/api/schedules",
      data,
    ),

  /** PUT /api/schedules/:id — update a job the caller owns. */
  update: (id: string, data: UpdateJobRequest) =>
    gateway.put<{ updated: boolean; job: ScheduledJob }>(
      `/api/schedules/${encodeURIComponent(id)}`,
      data,
    ),

  /** DELETE /api/schedules/:id — delete a job the caller owns. */
  delete: (id: string) =>
    gateway.del<{ deleted: boolean; id: string }>(
      `/api/schedules/${encodeURIComponent(id)}`,
    ),

  /** POST /api/schedules/:id/run — trigger a job immediately. */
  trigger: (id: string) =>
    gateway.post<{ triggered: boolean; run: JobRunRecord }>(
      `/api/schedules/${encodeURIComponent(id)}/run`,
    ),

  /** GET /api/schedules/:id/history — run history for a specific job. */
  jobHistory: (id: string) =>
    gateway.get<{ history: JobRunRecord[] }>(
      `/api/schedules/${encodeURIComponent(id)}/history`,
    ),
};
