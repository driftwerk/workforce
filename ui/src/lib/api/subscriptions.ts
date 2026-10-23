import { gateway } from "../api-client";

export type SubscriptionKind = "claude" | "chatgpt";

export type CredentialTier = "agent_user" | "global_user" | "agent_shared" | "system_shared";

export type SharedServes = "owner" | "agent_users" | "anyone";

export interface LoginAccount {
  email: string | null;
  plan: string | null;
}

export type LoginStatus =
  | { state: "connected"; account: LoginAccount | null }
  | { state: "cooling"; until: string }
  | { state: "invalid"; reason: string }
  | { state: "not_set" };

export interface LoginSlotView {
  kind: SubscriptionKind;
  tier: CredentialTier;
  agent_id: string | null;
  agent_name: string | null;
  status: LoginStatus;
  updated_at: string | null;
  updated_by: string | null;
}

export interface MySubscriptions {
  chatgpt_direct: boolean;
  logins: LoginSlotView[];
}

export interface LoginPolicyView {
  tiers: CredentialTier[];
  shared_serves: SharedServes;
  overridden: boolean;
}

export interface LoginMiss {
  tier: CredentialTier;
  reason: string;
}

export interface ServingView {
  kind: SubscriptionKind;
  tier: CredentialTier | null;
  misses: LoginMiss[];
}

export type PersonalTier = Extract<CredentialTier, "agent_user" | "global_user">;

export interface LoginUserView {
  kind: SubscriptionKind;
  tier: PersonalTier;
  display_name: string;
}

export interface AgentLoginsView {
  policy: LoginPolicyView;
  install: { chatgpt_direct: boolean; remote_worker: boolean };
  kinds: SubscriptionKind[];
  serving_you: ServingView[];
  shared: LoginSlotView[];
  can_manage_shared: boolean;
  can_manage_install: boolean;
  users: LoginUserView[] | null;
  route_options: string[];
}

export interface LoginTarget {
  tier: CredentialTier;
  agent_id?: string;
}

export type DeviceTier = Exclude<CredentialTier, "system_shared">;

export interface DeviceTarget {
  tier: DeviceTier;
  agent_id?: string;
}

export interface FlowTarget extends DeviceTarget {
  agent_name: string | null;
}

/** `target` is absent only on a flow the hub no longer knows. */
export type DeviceFlowView = { flow_id: string; target?: FlowTarget } & (
  | {
      state: "pending";
      user_code: string;
      verification_url: string;
      expires_at: string;
      interval_secs: number;
    }
  | { state: "completed"; account: LoginAccount }
  | { state: "expired" | "failed" | "cancelled" | "unsupported"; reason: string }
);

export interface ConnectPrompt {
  kind: SubscriptionKind;
  agent_id: string;
  flow_id?: string;
  reason: string;
  private: boolean;
  /** Whether the message the prompt answers is re-run once the sign-in
   *  completes. */
  resumes: boolean;
}

function targetQuery(target: LoginTarget): string {
  return new URLSearchParams({
    tier: target.tier,
    ...(target.agent_id ? { agent_id: target.agent_id } : {}),
  }).toString();
}

export const subscriptionsApi = {
  mine: () => gateway.get<MySubscriptions>("/api/me/subscriptions"),

  agentLogins: (agentId: string) =>
    gateway.get<AgentLoginsView>(`/api/agents/${encodeURIComponent(agentId)}/logins`),

  saveLogin: (kind: SubscriptionKind, target: LoginTarget, value: string) =>
    gateway.post<{ stored: CredentialTier }>(`/api/me/subscriptions/${kind}/login`, {
      ...target,
      value,
    }),

  removeLogin: (kind: SubscriptionKind, target: LoginTarget) =>
    gateway.del<{ removed: boolean }>(
      `/api/me/subscriptions/${kind}/login?${targetQuery(target)}`,
    ),

  startDeviceFlow: (target: DeviceTarget) =>
    gateway.post<DeviceFlowView>("/api/me/subscriptions/chatgpt/device", target),

  currentDeviceFlow: () => gateway.get<DeviceFlowView>("/api/me/subscriptions/chatgpt/device"),

  /** 404 for anyone but the person who started it. */
  deviceFlow: (flowId: string) =>
    gateway.get<DeviceFlowView>(
      `/api/me/subscriptions/chatgpt/device/${encodeURIComponent(flowId)}`,
    ),

  cancelDeviceFlow: (flowId: string) =>
    gateway.del<{ cancelled: string }>(
      `/api/me/subscriptions/chatgpt/device/${encodeURIComponent(flowId)}`,
    ),
};
