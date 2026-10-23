import { gateway } from "../api-client";

// ─── Sub-agent types ───

export interface SubAgentTask {
  task_id: string;
  task: string;
  status: "running" | "completed" | "failed" | "cancelled" | "timed_out";
  spawning_agent: string;
  model: string | null;
  started_at: string;
  finished_at: string | null;
  elapsed_secs: number | null;
  duration_secs?: number | null;
  result_preview?: string | null;
  error?: string | null;
}

export interface SubAgentsResponse {
  tasks: SubAgentTask[];
  count: number;
}

// ─── Event source types ───

export interface EventSource {
  id: string;
  name: string;
  source_type: string;
  status: string;
  events_received: number;
  last_event: string | null;
  started_at: string | null;
}

export interface EventSourcesResponse {
  sources: EventSource[];
  count: number;
}

// ─── Handoff types ───

export interface HandoffSummary {
  id: string;
  type: string;
  source_team: string;
  target_team: string;
  status: string;
  priority: string | null;
  summary: string;
  artifact_count: number;
}

export interface HandoffArtifact {
  id: string;
  title: string;
  type: string;
}

export interface HandoffDetail extends HandoffSummary {
  artifacts: HandoffArtifact[];
}

export interface HandoffsResponse {
  handoffs: HandoffSummary[];
  count: number;
}

export interface CreateHandoffRequest {
  id?: string;
  handoff_type: string;
  target_team: string;
  summary?: string;
  priority?: string;
  source_team?: string;
}

export interface UpdateHandoffRequest {
  summary?: string;
  priority?: string;
}

// ─── Knowledge index types ───

export interface KnowledgeStatus {
  repo_id: string;
  repo_name: string;
  workspace: string;
  indexed: boolean;
  indexing_in_progress: boolean;
  any_indexing_in_progress: boolean;
  last_commit: string | null;
  last_full_index_at: string | null;
  last_incremental_at: string | null;
  total_entities: number;
  total_relationships: number;
  total_files: number;
}

export interface TriggerIndexRequest {
  mode?: "incremental" | "full";
  path?: string;
}

export interface TriggerIndexResponse {
  repo_id: string;
  mode: string;
  status: string;
  message: string;
}

// ─── Audit log types ───

export interface AuditEntry {
  timestamp: string;
  actor: string;
  action: string;
  target: string;
  outcome: string;
  detail: string | null;
}

export interface AuditResponse {
  entries: AuditEntry[];
  total: number;
  offset: number;
  limit: number;
}

export interface AuditQuery {
  query?: string;
  limit?: number;
  offset?: number;
  from?: string;
  to?: string;
  agent?: string;
  action?: string;
  outcome?: string;
  format?: "json" | "csv";
}

// ─── System log types ───

export interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

export interface LogsResponse {
  entries: LogEntry[];
  count: number;
}

export interface LogsQuery {
  action?: "read" | "search";
  lines?: number;
  level?: string;
  target?: string;
  keyword?: string;
  since?: string;
  limit?: number;
}

// ─── Admin API client ───

export const adminApi = {
  // Sub-agent management
  subagents: {
    list: () => gateway.get<SubAgentsResponse>("/api/subagents"),
    get: (id: string) => gateway.get<SubAgentTask>(`/api/subagents/${id}`),
    cancel: (id: string) =>
      gateway.post<{ task_id: string; status: string }>(`/api/subagents/${id}/cancel`, {}),
  },

  // Event source management
  events: {
    listSources: () => gateway.get<EventSourcesResponse>("/api/events/sources"),
    startSource: (id: string) =>
      gateway.post<{ id: string; status: string; message?: string }>(
        `/api/events/sources/${id}/start`,
        {},
      ),
    stopSource: (id: string) =>
      gateway.post<{ id: string; status: string }>(`/api/events/sources/${id}/stop`, {}),
    restartSource: (id: string) =>
      gateway.post<{ id: string; status: string; message?: string }>(
        `/api/events/sources/${id}/restart`,
        {},
      ),
  },

  // Handoff management
  handoffs: {
    list: (team?: string, status?: string) => {
      const params = new URLSearchParams();
      if (team) params.set("team", team);
      if (status) params.set("status", status);
      const qs = params.toString();
      return gateway.get<HandoffsResponse>(`/api/handoffs${qs ? `?${qs}` : ""}`);
    },
    get: (id: string) => gateway.get<HandoffDetail>(`/api/handoffs/${id}`),
    create: (req: CreateHandoffRequest) =>
      gateway.post<HandoffSummary>("/api/handoffs", req),
    update: (id: string, req: UpdateHandoffRequest) =>
      gateway.put<{ id: string; summary: string; priority: string | null; updated: boolean }>(
        `/api/handoffs/${id}`,
        req,
      ),
    delete: (id: string) =>
      gateway.del<{ id: string; deleted: boolean }>(`/api/handoffs/${id}`),
    accept: (id: string) =>
      gateway.post<{ id: string; status: string }>(`/api/handoffs/${id}/accept`, {}),
  },

  // Knowledge index management
  knowledge: {
    status: (path?: string) => {
      const params = path ? `?path=${encodeURIComponent(path)}` : "";
      return gateway.get<KnowledgeStatus>(`/api/knowledge/status${params}`);
    },
    triggerIndex: (req: TriggerIndexRequest) =>
      gateway.post<TriggerIndexResponse>("/api/knowledge/index", req),
    cancelIndex: (path?: string) => {
      const params = path ? `?path=${encodeURIComponent(path)}` : "";
      return gateway.post<{ repo_id: string; cancelled: boolean }>(
        `/api/knowledge/index/cancel${params}`,
        {},
      );
    },
  },

  // Audit log (enhanced)
  audit: {
    list: (query?: AuditQuery) => {
      const params = new URLSearchParams();
      if (query) {
        for (const [k, v] of Object.entries(query)) {
          if (v !== undefined && v !== null) params.set(k, String(v));
        }
      }
      const qs = params.toString();
      return gateway.get<AuditResponse>(`/api/audit${qs ? `?${qs}` : ""}`);
    },
    exportCsv: (query?: Omit<AuditQuery, "format">) => {
      const params = new URLSearchParams({ format: "csv" });
      if (query) {
        for (const [k, v] of Object.entries(query)) {
          if (v !== undefined && v !== null) params.set(k, String(v));
        }
      }
      return gateway.get<string>(`/api/audit?${params}`);
    },
  },

  // System logs
  logs: {
    read: (lines?: number) => {
      const params = lines ? `?lines=${lines}` : "";
      return gateway.get<LogsResponse>(`/api/logs${params}`);
    },
    search: (query: Omit<LogsQuery, "action">) => {
      const params = new URLSearchParams({ action: "search" });
      for (const [k, v] of Object.entries(query)) {
        if (v !== undefined && v !== null) params.set(k, String(v));
      }
      return gateway.get<LogsResponse>(`/api/logs?${params}`);
    },
  },
};
