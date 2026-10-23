import type { CredentialTier, SharedServes } from "@/lib/api/subscriptions";

/** `[llm.logins]` as an agent's config.toml spells it. */
export interface LoginPolicy {
  tiers: CredentialTier[];
  shared_serves: SharedServes;
}

export const ALL_TIERS: readonly CredentialTier[] = [
  "agent_user",
  "global_user",
  "agent_shared",
  "system_shared",
];

export const SHARED_SERVES: readonly SharedServes[] = ["owner", "agent_users", "anyone"];

/** The policy an agent runs on when nothing sets one. */
export const DEFAULT_POLICY: LoginPolicy = { tiers: [...ALL_TIERS], shared_serves: "anyone" };

const isTier = (value: unknown): value is CredentialTier =>
  ALL_TIERS.includes(value as CredentialTier);

const isServes = (value: unknown): value is SharedServes =>
  SHARED_SERVES.includes(value as SharedServes);

/** The `[llm.logins]` block a config sets, with the server's defaults for a
 *  key it leaves out; `null` when the config sets none. */
export function readPolicy(block: unknown): LoginPolicy | null {
  if (block == null || typeof block !== "object" || Array.isArray(block)) return null;
  const { tiers, shared_serves } = block as Record<string, unknown>;
  return {
    tiers: Array.isArray(tiers) ? tiers.filter(isTier) : [...DEFAULT_POLICY.tiers],
    shared_serves: isServes(shared_serves) ? shared_serves : DEFAULT_POLICY.shared_serves,
  };
}

/** An override that starts as the policy in effect, both keys written. */
export function seedOverride(effective: LoginPolicy): LoginPolicy {
  return { tiers: [...effective.tiers], shared_serves: effective.shared_serves };
}

/** Whether `tier` may be left out: a policy names at least one tier. */
export function canExclude(policy: LoginPolicy, tier: CredentialTier): boolean {
  return !(policy.tiers.length === 1 && policy.tiers[0] === tier);
}

/** `tier` in or out of the order; a newly included tier is tried last. */
export function toggleTier(policy: LoginPolicy, tier: CredentialTier): LoginPolicy {
  if (!policy.tiers.includes(tier)) return { ...policy, tiers: [...policy.tiers, tier] };
  if (!canExclude(policy, tier)) return policy;
  return { ...policy, tiers: policy.tiers.filter((t) => t !== tier) };
}

/** `tier` one place earlier (`-1`) or later (`1`) in the order. */
export function moveTier(policy: LoginPolicy, tier: CredentialTier, by: -1 | 1): LoginPolicy {
  const from = policy.tiers.indexOf(tier);
  const to = from + by;
  if (from < 0 || to < 0 || to >= policy.tiers.length) return policy;
  const tiers = [...policy.tiers];
  [tiers[from], tiers[to]] = [tiers[to]!, tiers[from]!];
  return { ...policy, tiers };
}

/** Every tier as a row: the included ones in the order they are tried, then
 *  the rest. */
export function tierRows(policy: LoginPolicy): { tier: CredentialTier; position: number | null }[] {
  return [
    ...policy.tiers.map((tier, position) => ({ tier, position })),
    ...ALL_TIERS.filter((tier) => !policy.tiers.includes(tier)).map((tier) => ({
      tier,
      position: null,
    })),
  ];
}
