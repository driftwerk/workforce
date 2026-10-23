import { gateway } from "../api-client";

/** One row of the hub's document store. */
export interface StoreDocument<T = Record<string, unknown>> {
  scope: { kind: "user" | "agent" | "global"; id?: string };
  collection: string;
  doc_id: string;
  body: T;
  version: number;
  created_by?: string;
  created_at: string;
  updated_at: string;
  expires_at?: string;
  lease_owner?: string;
  lease_expires_at?: string;
}

export interface StorePage<T = Record<string, unknown>> {
  scope: string;
  collection: string;
  count: number;
  documents: StoreDocument<T>[];
  next_cursor?: string | null;
}

export interface StoreListParams {
  limit?: number;
  cursor?: string;
  direction?: "asc" | "desc";
  order_by?: string;
}

/**
 * `scope` is spelled as the routes spell it: `user` (the caller's own),
 * `user:<id>`, `agent:<id>` or `global`.
 */
export const storeApi = {
  list: <T = Record<string, unknown>>(scope: string, collection: string, params: StoreListParams = {}) => {
    const qs = new URLSearchParams();
    if (params.limit) qs.set("limit", String(params.limit));
    if (params.cursor) qs.set("cursor", params.cursor);
    if (params.direction) qs.set("direction", params.direction);
    if (params.order_by) qs.set("order_by", params.order_by);
    const q = qs.toString();
    return gateway.get<StorePage<T>>(
      `/api/store/${encodeURIComponent(scope)}/${encodeURIComponent(collection)}${q ? `?${q}` : ""}`,
    );
  },
  query: <T = Record<string, unknown>>(scope: string, collection: string, query: Record<string, unknown>) =>
    gateway.post<StorePage<T>>(
      `/api/store/${encodeURIComponent(scope)}/${encodeURIComponent(collection)}`,
      query,
    ),
  get: <T = Record<string, unknown>>(scope: string, collection: string, docId: string) =>
    gateway.get<StoreDocument<T>>(
      `/api/store/${encodeURIComponent(scope)}/${encodeURIComponent(collection)}/${encodeURIComponent(docId)}`,
    ),
};
