import { useQuery } from "@tanstack/react-query";
import { toolsApi } from "@/lib/api/tools";

export function useTools() {
  return useQuery({
    queryKey: ["tools", "global"],
    queryFn: () => toolsApi.listGlobal(),
    staleTime: 60_000,
  });
}
