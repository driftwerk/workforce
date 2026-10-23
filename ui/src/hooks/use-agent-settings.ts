import { useQuery } from "@tanstack/react-query";
import { agentSettingsApi } from "@/lib/api/agent-settings";
import type { AgentSettingsResponse } from "@/lib/api/agent-settings";

export type {
  AgentSettingsResponse,
  SettingLayer,
  SettingRow,
  SettingSource,
  SettingStatus,
} from "@/lib/api/agent-settings";

export function useAgentSettings(agentId: string | undefined) {
  return useQuery<AgentSettingsResponse>({
    queryKey: ["agent-settings", agentId],
    queryFn: () => agentSettingsApi.get(agentId!),
    enabled: !!agentId,
    staleTime: 15_000,
  });
}
