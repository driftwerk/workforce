import { Badge } from "@/components/ui/badge";
import type { VaultTier } from "@/lib/api/vault";

const LABELS: Record<VaultTier, string> = {
  agent_per_user: "Agent · per user",
  global_per_user: "Global · per user",
  agent_specific: "Agent specific",
  global: "Global",
};

const TONE: Record<VaultTier, string> = {
  agent_per_user: "bg-violet-500 hover:bg-violet-600 text-white",
  global_per_user: "bg-emerald-600 hover:bg-emerald-700 text-white",
  agent_specific: "bg-sky-600 hover:bg-sky-700 text-white",
  global: "bg-amber-600 hover:bg-amber-700 text-white",
};

export function TierBadge({ tier }: { tier: VaultTier }) {
  return (
    <Badge className={TONE[tier]} title={LABELS[tier]}>
      {LABELS[tier]}
    </Badge>
  );
}

export function tierLabel(tier: VaultTier): string {
  return LABELS[tier];
}
