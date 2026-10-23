import { gateway } from "../api-client";
import type { PrincipalKind } from "./cost";

/** Structured, human-readable view of a session's opaque `channel` string,
 *  built server-side (see `channel_display.rs`). `raw` is shown on hover. */
export type ChannelKind = "dm" | "channel" | "group" | "other";

export interface ChannelDisplay {
  platform: string;
  workspace?: string;
  name: string;
  kind: ChannelKind;
  thread: boolean;
  raw: string;
}

export interface Session {
  id: string;
  label: string | null;
  channel: string;
  channel_display?: ChannelDisplay;
  /** True for Claude Code sessions, whose transcript lives in the subprocess
   *  and is intentionally not mirrored here. */
  is_claude_chat?: boolean;
  owner: string;
  owner_display_name: string;
  owner_kind: PrincipalKind;
  agent_id: string;
  agent_display_name: string;
  /** Whether this is a user-driven or system-driven session. Drives
   *  the "user vs system" filter in the Sessions UI. */
  session_category: "user" | "system";
  kind: string;
  messages: number;
  total_tokens: number;
  created_at: string;
  updated_at: string;
  thread_url?: string;
  slack_channel_id?: string;
}

export interface TranscriptEntry {
  /** Unique entry id — used as the backward-pagination cursor. */
  id: string;
  /** Session this entry belongs to — the cursor key across a rolled-over chain. */
  session_id: string;
  role: string;
  content: string;
  timestamp?: string;
  tool_metadata?: { name: string; success: boolean };
}

export interface ToolStats {
  count: number;
  succeeded: number;
  failed: number;
  total_duration_ms: number;
}

export interface ToolCallSummary {
  total: number;
  succeeded: number;
  failed: number;
  total_duration_ms: number;
  by_tool: Record<string, ToolStats>;
}

export interface SessionStats {
  message_count: number;
  messages_by_role: Record<string, number>;
  tokens_by_role: Record<string, number>;
  total_tokens: number;
  total_input_tokens: number;
  total_output_tokens: number;
  tool_calls: ToolCallSummary;
  duration_seconds?: number | null;
}

export const sessionsApi = {
  list: (agentId?: string, channel?: string, conversationalOnly = false) => {
    const params = new URLSearchParams();
    if (agentId) params.set("agent_id", agentId);
    if (channel) params.set("channel", channel);
    if (conversationalOnly) params.set("conversational_only", "true");
    return gateway.get<{ sessions: Session[] }>(`/api/sessions?${params}`);
  },
  transcript: (id: string, opts?: { before?: string; beforeSession?: string; limit?: number }) => {
    const params = new URLSearchParams();
    if (opts?.before) params.set("before", opts.before);
    if (opts?.beforeSession) params.set("before_session", opts.beforeSession);
    if (opts?.limit) params.set("limit", String(opts.limit));
    const qs = params.toString();
    return gateway.get<{ session_id: string; entries: TranscriptEntry[]; has_more: boolean }>(
      `/api/sessions/${id}/transcript${qs ? `?${qs}` : ""}`,
    );
  },
  stats: (id: string) => gateway.get<{ session_id: string; stats: SessionStats }>(`/api/sessions/${id}/stats`),
  export: (id: string) => gateway.get<Record<string, unknown>>(`/api/sessions/${id}/export`),
};
