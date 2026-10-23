import { useQuery } from "@tanstack/react-query";
import { dashboardApi } from "@/lib/api/dashboard";

export function useDashboard() {
  return useQuery({ queryKey: ["dashboard"], queryFn: dashboardApi.get, staleTime: 10_000, refetchInterval: 30_000 });
}

export function useHealth() {
  return useQuery({ queryKey: ["health"], queryFn: dashboardApi.health, staleTime: 30_000 });
}
