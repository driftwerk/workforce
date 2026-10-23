import type {
  CredentialTier,
  LoginAccount,
  LoginSlotView,
  LoginStatus,
  PersonalTier,
  SharedServes,
  SubscriptionKind,
} from "@/lib/api/subscriptions";

export const KIND_LABELS: Record<SubscriptionKind, string> = {
  claude: "Claude",
  chatgpt: "ChatGPT",
};

export const TIER_LABELS: Record<CredentialTier, string> = {
  agent_user: "Your own login on this agent",
  global_user: "Your own login on every agent",
  agent_shared: "This agent's shared login",
  system_shared: "The install's own login",
};

/** Where someone else's own login is kept, as a list of them reads it. */
export const HOLDER_LABELS: Record<PersonalTier, string> = {
  agent_user: "on this agent",
  global_user: "on every agent",
};

export const CHATGPT_DIRECT_OFF =
  "ChatGPT agent turns are off on this install; the login still serves Codex.";

export const SERVES_LABELS: Record<SharedServes, string> = {
  owner: "The agent's owner and its background work",
  agent_users: "Everyone who talks to the agent",
  anyone: "Anyone, including unidentified callers",
};

const capitalise = (word: string) => word.charAt(0).toUpperCase() + word.slice(1);

/** "you@x (Plus)", as much of it as the sign-in names. */
export function accountLabel(account: LoginAccount): string | null {
  const plan = account.plan ? `(${capitalise(account.plan)})` : null;
  return [account.email, plan].filter(Boolean).join(" ") || null;
}

const sameDay = (a: Date, b: Date) => a.toDateString() === b.toDateString();

/** A local time, with its date when it isn't today. */
export function localMoment(at: Date, now: Date): string {
  const time = at.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  return sameDay(at, now)
    ? time
    : `${at.toLocaleDateString(undefined, { day: "numeric", month: "short" })} ${time}`;
}

export function ownLoginStatus(
  logins: LoginSlotView[],
  kind: SubscriptionKind,
  tier: PersonalTier,
  agentId?: string,
): LoginStatus {
  const slot = logins.find(
    (login) => login.kind === kind && login.tier === tier && (login.agent_id ?? undefined) === agentId,
  );
  return slot?.status ?? { state: "not_set" };
}

export function loginStatusLabel(status: LoginStatus, now: Date): string {
  switch (status.state) {
    case "connected": {
      const account = status.account && accountLabel(status.account);
      return account ? `Connected · ${account}` : "Connected";
    }
    case "cooling":
      return `At limit until ${localMoment(new Date(status.until), now)}`;
    case "invalid":
      return "Signed out: reconnect";
    case "not_set":
      return "Not set";
  }
}

export function loginStatusVariant(
  status: LoginStatus,
): "success" | "warning" | "destructive" | "outline" {
  switch (status.state) {
    case "connected":
      return "success";
    case "cooling":
      return "warning";
    case "invalid":
      return "destructive";
    case "not_set":
      return "outline";
  }
}
