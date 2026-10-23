import { admin } from "../api-client";

export interface AuditEntry {
  timestamp: string;
  agent_id: string;
  action: string;
  detail: string;
}

export const auditApi = {
  list: (query?: string, limit?: number) => {
    const params = new URLSearchParams();
    if (query) params.set("query", query);
    if (limit) params.set("limit", limit.toString());
    return admin.get<AuditEntry[]>(`/api/audit?${params}`);
  },
};
