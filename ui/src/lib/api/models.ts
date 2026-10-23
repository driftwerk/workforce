import { gateway } from "../api-client";

export const modelsApi = {
  health: () => gateway.get<{ enabled: boolean; models: Record<string, unknown> }>("/api/models/health"),
  aliases: () => gateway.get<{ default_model: string; failover_chain: string[]; aliases: Record<string, string> }>("/api/models/aliases"),
  reset: (agentId?: string) => {
    const params = agentId ? `?agent_id=${agentId}` : "";
    return gateway.post<{ model: string }>(`/api/admin/reset-model${params}`);
  },
};
