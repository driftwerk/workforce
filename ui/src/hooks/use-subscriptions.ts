import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ApiError } from "@/lib/api-client";
import {
  subscriptionsApi,
  type DeviceFlowView,
  type DeviceTarget,
  type LoginTarget,
  type SubscriptionKind,
} from "@/lib/api/subscriptions";
import { nextFlowUi, pollFlow } from "@/lib/subscriptions/device-flow";

const CURRENT_FLOW_KEY = ["device-flow-current"];

function invalidateLogins(qc: QueryClient) {
  for (const queryKey of [["my-subscriptions"], ["my-integrations"], ["agent-logins"], CURRENT_FLOW_KEY]) {
    qc.invalidateQueries({ queryKey });
  }
}

export function useMySubscriptions() {
  return useQuery({
    queryKey: ["my-subscriptions"],
    queryFn: subscriptionsApi.mine,
    staleTime: 10_000,
  });
}

export function useAgentLogins(agentId: string | undefined) {
  return useQuery({
    queryKey: ["agent-logins", agentId],
    queryFn: () => subscriptionsApi.agentLogins(agentId!),
    enabled: !!agentId,
    staleTime: 10_000,
  });
}

export function useSaveLogin() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kind, target, value }: { kind: SubscriptionKind; target: LoginTarget; value: string }) =>
      subscriptionsApi.saveLogin(kind, target, value),
    onSuccess: () => invalidateLogins(qc),
  });
}

export function useRemoveLogin() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kind, target }: { kind: SubscriptionKind; target: LoginTarget }) =>
      subscriptionsApi.removeLogin(kind, target),
    onSuccess: () => invalidateLogins(qc),
  });
}

export function useCurrentDeviceFlow() {
  return useQuery({
    queryKey: CURRENT_FLOW_KEY,
    queryFn: async (): Promise<DeviceFlowView | null> => {
      try {
        return await subscriptionsApi.currentDeviceFlow();
      } catch (error) {
        if (error instanceof ApiError && error.status === 404) return null;
        throw error;
      }
    },
    staleTime: 10_000,
  });
}

export function useStartDeviceFlow(target: DeviceTarget) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => subscriptionsApi.startDeviceFlow(target),
    onSuccess: (flow) => {
      qc.setQueryData(["device-flow", flow.flow_id], flow);
      qc.setQueryData(CURRENT_FLOW_KEY, flow);
    },
  });
}

/** A sign-in that completed refreshes every view of the caller's logins;
 *  one that ended otherwise — replaced, cancelled, expired — refreshes which
 *  sign-in the caller keeps. */
export function refreshOnSettle(qc: QueryClient, state: DeviceFlowView["state"] | undefined) {
  if (state === "completed") invalidateLogins(qc);
  else if (state && state !== "pending") qc.invalidateQueries({ queryKey: CURRENT_FLOW_KEY });
}

/** One sign-in, polled at its own interval while it waits for its code. */
export function useDeviceFlow(flowId: string | null | undefined) {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: ["device-flow", flowId],
    queryFn: () => pollFlow(subscriptionsApi.deviceFlow, flowId!),
    enabled: !!flowId,
    refetchInterval: (q) => nextFlowUi(q.state.data, false).pollMs,
    retry: false,
  });
  const state = query.data?.state;
  useEffect(() => refreshOnSettle(qc, state), [state, qc]);
  return query;
}

export function useCancelDeviceFlow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (flowId: string) => subscriptionsApi.cancelDeviceFlow(flowId),
    onSettled: (_, __, flowId) => {
      qc.invalidateQueries({ queryKey: ["device-flow", flowId] });
      qc.invalidateQueries({ queryKey: CURRENT_FLOW_KEY });
    },
  });
}
