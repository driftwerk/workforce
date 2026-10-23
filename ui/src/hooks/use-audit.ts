import { useQuery } from "@tanstack/react-query";
import { auditApi } from "@/lib/api/audit";

export function useAuditLog(query?: string, limit?: number) {
  return useQuery({ queryKey: ["audit", query, limit], queryFn: () => auditApi.list(query, limit), staleTime: 10_000 });
}
