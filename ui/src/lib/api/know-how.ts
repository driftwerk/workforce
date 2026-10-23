import { gateway } from "../api-client";

export type KnowHowKind =
  | "data_location"
  | "query_shape"
  | "schema_correction"
  | "code_location";

/** Where an entry stands for everyone but the people who produced it. */
export type KnowHowStatus = "pinned" | "shared" | "pending" | "private" | "stale";

/**
 * One learned entry as an admin sees it: counts, and the names others see in it, never who or
 * why. With `names_hidden` its names and times are one person's and are left out.
 */
export interface KnowHowEntry {
  id: number;
  kind: KnowHowKind;
  status: KnowHowStatus;
  names_hidden: boolean;
  site?: string;
  database_id?: number;
  tables: string[];
  columns: string[];
  shape: string | null;
  wrong: string | null;
  repo: string | null;
  path: string | null;
  symbol: string | null;
  note: string | null;
  manual: boolean;
  people: number;
  uses: number;
  first_seen: string | null;
  last_seen: string | null;
}

export interface KnowHowListResponse {
  agent_id: string;
  min_users: number;
  entries: KnowHowEntry[];
}

export interface KnowHowActionOutcome {
  id: number;
  done: "pin" | "retire" | "delete";
}

export interface KnowHowEraseOutcome {
  identity_id: string;
  entries_touched: number;
}

function entriesPath(agentId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/know-how`;
}

export const knowHowApi = {
  list: (agentId: string) => gateway.get<KnowHowListResponse>(entriesPath(agentId)),

  pin: (agentId: string, id: number) =>
    gateway.post<KnowHowActionOutcome>(`${entriesPath(agentId)}/${id}/pin`),

  retire: (agentId: string, id: number) =>
    gateway.post<KnowHowActionOutcome>(`${entriesPath(agentId)}/${id}/retire`),

  remove: (agentId: string, id: number) =>
    gateway.del<KnowHowActionOutcome>(`${entriesPath(agentId)}/${id}`),

  eraseIdentity: (identityId: string) =>
    gateway.del<KnowHowEraseOutcome>(
      `/api/identities/${encodeURIComponent(identityId)}/know-how`,
    ),
};
