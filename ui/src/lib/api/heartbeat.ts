import { gateway } from "../api-client";

// ── Types ─────────────────────────────────────────────────────────────────

export interface HeartbeatCheckSummary {
  id: string;
  name: string;
  enabled: boolean;
  schedule: string | null;
  priority: number;
  tags: string[];
}

export interface HeartbeatCheckDetail extends HeartbeatCheckSummary {
  timezone: string | null;
  cooldown_secs: number | null;
  body: string;
}

export interface CreateHeartbeatCheckRequest {
  id: string;
  name: string;
  body: string;
  schedule?: string;
  priority?: number;
  tags?: string[];
  timezone?: string;
  cooldown_secs?: number;
}

export interface UpdateHeartbeatCheckRequest {
  name?: string;
  body?: string;
  schedule?: string;
  priority?: number;
  enabled?: boolean;
  tags?: string[];
}

export interface HeartbeatHistoryRecord {
  check_id: string;
  check_name: string;
  timestamp: string;
  actionable: boolean;
  delivered: boolean;
  response: string;
  duration_ms: number;
  error: string | null;
}

// ── API Client ────────────────────────────────────────────────────────────

export const heartbeatApi = {
  /** GET /api/heartbeat — overall heartbeat system status. */
  status: () => gateway.get<{ enabled: boolean; interval_secs?: number; last_run?: string }>("/api/heartbeat"),

  /** POST /api/heartbeat/wake — trigger an immediate heartbeat wake. */
  wake: () => gateway.post<{ success: boolean }>("/api/heartbeat/wake"),

  /** GET /api/heartbeat/checks — list all heartbeat checks. */
  listChecks: () =>
    gateway.get<{ checks: HeartbeatCheckSummary[] }>("/api/heartbeat/checks"),

  /** POST /api/heartbeat/checks — create a new heartbeat check. */
  createCheck: (data: CreateHeartbeatCheckRequest) =>
    gateway.post<{ created: boolean; id: string; name: string }>(
      "/api/heartbeat/checks",
      data,
    ),

  /** GET /api/heartbeat/checks/:id — get full detail of a heartbeat check. */
  getCheck: (id: string) =>
    gateway.get<HeartbeatCheckDetail>(`/api/heartbeat/checks/${encodeURIComponent(id)}`),

  /** PUT /api/heartbeat/checks/:id — update an existing heartbeat check. */
  updateCheck: (id: string, data: UpdateHeartbeatCheckRequest) =>
    gateway.put<{ updated: boolean; id: string; name: string }>(
      `/api/heartbeat/checks/${encodeURIComponent(id)}`,
      data,
    ),

  /** DELETE /api/heartbeat/checks/:id — delete a heartbeat check. */
  deleteCheck: (id: string) =>
    gateway.del<{ deleted: boolean; id: string }>(
      `/api/heartbeat/checks/${encodeURIComponent(id)}`,
    ),

  /** POST /api/heartbeat/checks/:id/trigger — trigger a specific check. */
  triggerCheck: (id: string) =>
    gateway.post<{ triggered: boolean; check_id: string }>(
      `/api/heartbeat/checks/${encodeURIComponent(id)}/trigger`,
    ),

  /** GET /api/heartbeat/history — get heartbeat run history. */
  history: () =>
    gateway.get<{ history: HeartbeatHistoryRecord[] }>("/api/heartbeat/history"),
};
