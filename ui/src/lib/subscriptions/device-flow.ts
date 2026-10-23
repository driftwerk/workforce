import { ApiError } from "@/lib/api-client";
import type {
  ConnectPrompt,
  DeviceFlowView,
  DeviceTarget,
  FlowTarget,
  LoginAccount,
} from "@/lib/api/subscriptions";

export type PendingFlow = Extract<DeviceFlowView, { state: "pending" }>;

/** What the sign-in card shows and does for where a flow stands. */
export interface FlowUi {
  canStart: boolean;
  pollMs: number | false;
  pending: PendingFlow | null;
  connected: LoginAccount | null;
  failure: string | null;
  showPaste: boolean;
}

export function nextFlowUi(flow: DeviceFlowView | null | undefined, starting: boolean): FlowUi {
  const pending = flow?.state === "pending" ? flow : null;
  return {
    canStart: !starting && !pending,
    pollMs: pending ? pending.interval_secs * 1000 : false,
    pending,
    connected: flow?.state === "completed" ? flow.account : null,
    failure: flow && "reason" in flow ? flow.reason : null,
    showPaste: flow?.state === "unsupported",
  };
}

/** Whole seconds until the server's `expires_at`, never below zero. */
export function secondsLeft(flow: PendingFlow, now: Date): number {
  return Math.max(0, Math.floor((Date.parse(flow.expires_at) - now.getTime()) / 1000));
}

export function countdownLabel(seconds: number): string {
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

/** A poll of `flowId`: a flow the hub no longer knows (or that isn't the
 *  caller's) reads as expired. */
export async function pollFlow(
  fetchFlow: (flowId: string) => Promise<DeviceFlowView>,
  flowId: string,
): Promise<DeviceFlowView> {
  try {
    return await fetchFlow(flowId);
  } catch (error) {
    if (error instanceof ApiError && error.status === 404) {
      return { state: "expired", flow_id: flowId, reason: "the sign-in is no longer available" };
    }
    throw error;
  }
}

/** What a sign-in signs in to, as "for …" reads it. */
export function flowTargetLabel(target: FlowTarget): string {
  const agent = target.agent_name ?? target.agent_id;
  switch (target.tier) {
    case "global_user":
      return "for every agent";
    case "agent_user":
      return `for ${agent}`;
    case "agent_shared":
      return `for ${agent}'s shared login`;
  }
}

/** The caller's kept sign-in as one card shows it: refreshed by its poll,
 *  and once settled only when this card started it or saw it waiting. */
export function watchedFlow(
  kept: DeviceFlowView | null | undefined,
  polled: DeviceFlowView | undefined,
  watched: string | null,
): DeviceFlowView | null {
  const flow = kept && polled?.flow_id === kept.flow_id ? { target: kept.target, ...polled } : (kept ?? null);
  return flow && (flow.state === "pending" || flow.flow_id === watched) ? flow : null;
}

const sameTarget = (a: DeviceTarget, b: DeviceTarget) =>
  a.tier === b.tier && (a.agent_id ?? null) === (b.agent_id ?? null);

/** What a Connect for `target` makes of the caller's one kept sign-in.
 *  `shown`: the sign-in if it is for `target`; one waiting for something
 *  else shows only on the global card, where My Settings keeps every code.
 *  `replaces`: what a new start would replace — a sign-in waiting for
 *  something else, which the person is warned of first. */
export interface ConnectView {
  shown: DeviceFlowView | null;
  replaces: FlowTarget | null;
  canStart: boolean;
}

export function connectView(
  kept: DeviceFlowView | null | undefined,
  target: DeviceTarget,
  starting: boolean,
): ConnectView {
  const pending = kept?.state === "pending";
  const elsewhere = kept?.target && !sameTarget(kept.target, target) ? kept.target : null;
  return {
    shown: kept && (!elsewhere || (pending && target.tier === "global_user")) ? kept : null,
    replaces: pending ? elsewhere : null,
    canStart: !starting && !(pending && !elsewhere),
  };
}

/** What a connect card says follows a completed sign-in: the message re-run,
 *  only when one waits on it. */
export function afterConnect(prompt: ConnectPrompt): string | undefined {
  return prompt.resumes ? "Re-running your message…" : undefined;
}
