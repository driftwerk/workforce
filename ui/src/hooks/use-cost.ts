import { useQuery } from "@tanstack/react-query";
import {
  costApi,
  type CostDetailParams,
  type ScheduledJobHistoryParams,
} from "@/lib/api/cost";

export function useCostMonthly(months = 12) {
  return useQuery({
    queryKey: ["cost-monthly", months],
    queryFn: () => costApi.monthly(months),
    staleTime: 5 * 60_000,
  });
}

export function useCostSummary() {
  return useQuery({ queryKey: ["cost"], queryFn: costApi.summary, staleTime: 15_000 });
}

export function useCostHistory(days: number) {
  return useQuery({
    queryKey: ["cost-history", days],
    queryFn: () => costApi.history(days),
    staleTime: 60_000,
  });
}

export function useCostDetail(params: CostDetailParams) {
  return useQuery({
    queryKey: ["cost-detail", params],
    queryFn: () => costApi.detail(params),
    staleTime: 15_000,
  });
}

export function useScheduledJobHistory(params: ScheduledJobHistoryParams) {
  return useQuery({
    queryKey: ["scheduled-job-history", params],
    queryFn: () => costApi.scheduledJobHistory(params),
    staleTime: 15_000,
  });
}
