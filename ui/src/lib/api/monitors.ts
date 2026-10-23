import { gateway } from "../api-client";
import type { DefRevision } from "./revisions";

/** Why a monitor is or is not watching right now (matches the backend MonitorStatus). */
export type MonitorStatus =
  | "active"
  | "paused"
  | "awaiting_invite"
  | "awaiting_owner"
  | "unknown_agent"
  | "failing";

/** What a monitor watches: a chat channel, or a source polled through a tool. */
export type MonitorKind = "channel" | "source";

/** The watching agent's bot cannot read the channel (matches the backend ChannelBlock). */
export interface MonitorBlock {
  agent: string;
  surface: string;
  channel: string;
  error: string;
  since: string;
}

/** A source monitor's last poll (matches the backend PollStatus). */
export interface MonitorPoll {
  polled_at: string | null;
  next_due_at: string;
  /** Absent when the last poll succeeded. */
  error?: string;
  failures: number;
}

/** What a monitor watches, by kind: a surface and channel, or a polled tool. */
export type MonitorWatch =
  | { kind: "channel"; surface: string; channel: string; source?: undefined }
  | { kind: "source"; source: string; surface?: undefined; channel?: undefined };

/** Summary type for the list endpoint (matches the backend MonitorSummary). */
export type MonitorDef = MonitorWatch & {
  id: string;
  name: string;
  description: string;
  /** The filter's kind, or the condition's. */
  filter: string;
  action: string;
  enabled: boolean;
  /** Owner identity; omitted/null for system/global definitions. */
  owner?: string | null;
  /** The stored word: `"private"` (owner only), `"team"` (anyone in a team
   *  with the owner) or `"global"` (anyone may run). */
  visibility?: string;
  /** The agent whose engine runs it; null when the hub has no agents configured. */
  agent?: string | null;
  status: MonitorStatus;
  block?: MonitorBlock | null;
  poll?: MonitorPoll | null;
};

/** One recent dispatch (matches the backend DispatchActivity). */
export interface MonitorActivity {
  message_id: string;
  channel: string;
  text: string;
  claimed_at: string;
  completed_at?: string | null;
  instance_id?: string | null;
  error?: string | null;
}

/** `?owner=<id>` query segment, or empty for a system/global definition. */
function ownerQuery(owner?: string | null): string {
  return owner ? `?owner=${encodeURIComponent(owner)}` : "";
}

export const monitorsApi = {
  // Member-visible: the caller sees global monitors plus their own.
  list: () => gateway.get<{ monitors: MonitorDef[] }>("/api/monitors"),
  getSource: (id: string, owner?: string | null) =>
    gateway.get<string>(`/api/monitors/${id}/source${ownerQuery(owner)}`),
  /** `from` renames (old id deleted after save); `visibility` sets private/team/global. */
  save: (
    id: string,
    toml: string,
    from?: string,
    visibility?: string,
    owner?: string | null,
  ) => {
    const params = new URLSearchParams();
    if (from && from !== id) params.set("from", from);
    if (visibility) params.set("visibility", visibility);
    if (owner) params.set("owner", owner);
    const qs = params.toString();
    return gateway.putText<{ saved: string }>(
      `/api/monitors/${id}${qs ? `?${qs}` : ""}`,
      toml,
    );
  },
  del: (id: string, owner?: string | null) =>
    gateway.del<{ deleted: string }>(`/api/monitors/${id}${ownerQuery(owner)}`),
  export: () => gateway.get<Record<string, string>>("/api/monitors/export"),
  activity: (id: string, owner?: string | null) =>
    gateway.get<{ activity: MonitorActivity[] }>(`/api/monitors/${id}/activity${ownerQuery(owner)}`),
  listRevisions: (id: string, owner?: string | null) =>
    gateway.get<{ revisions: DefRevision[] }>(`/api/monitors/${id}/revisions${ownerQuery(owner)}`),
  getRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.get<DefRevision>(`/api/monitors/${id}/revisions/${rev}${ownerQuery(owner)}`),
  restoreRevision: (id: string, rev: number, owner?: string | null) =>
    gateway.post<{ restored: string; from_rev: number }>(
      `/api/monitors/${id}/revisions/${rev}/restore${ownerQuery(owner)}`,
    ),
};
