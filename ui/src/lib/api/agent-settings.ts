import { gateway } from "../api-client";

/** The layer a value comes from, lowest first. */
export type SettingSource =
  | "default"
  | "install"
  | "install_agent"
  | "agent"
  | "runtime"
  | "environment"
  | "vault";

export type SettingStatus = "active" | "off" | "kill_switched" | "shadow" | "inert";

export interface SettingLayer {
  source: SettingSource;
  value: unknown;
}

/** One effective setting. A credential's value is only ever "set" or "not set". */
export interface SettingRow {
  section: string;
  key: string;
  value: unknown;
  source?: SettingSource;
  status?: SettingStatus;
  reason?: string;
  /** Every layer that sets the value, lowest first. */
  chain?: SettingLayer[];
}

export interface AgentSettingsResponse {
  agent: string;
  /** "admin" when install-wide rows are included. */
  viewer: "admin" | "member";
  sections: string[];
  rows: SettingRow[];
}

/** Whether a row says more than "this is the built-in default, in effect". */
export const isNotable = (row: SettingRow): boolean =>
  row.source !== "default" || row.status !== undefined;

export const agentSettingsApi = {
  get: (agentId: string) =>
    gateway.get<AgentSettingsResponse>(`/api/agents/${encodeURIComponent(agentId)}/settings`),
};
