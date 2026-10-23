import { Badge } from "@/components/ui/badge";
import type { CapabilityDecisionValue } from "@/lib/api/permissions";

const ROLE_LABEL: Record<string, string> = {
  member: "Member",
  manager: "Manager",
  system_admin: "System admin",
};

/** Title-case an unknown snake_case key as a last-resort human label. */
export function humanizeKey(key: string): string {
  return key
    .split("_")
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

/** A role's display label — built-in names, else the humanized key. */
export function roleLabelFallback(key: string): string {
  return ROLE_LABEL[key] ?? humanizeKey(key);
}

const DECISION_TONE: Record<
  CapabilityDecisionValue,
  "secondary" | "outline" | "destructive"
> = {
  allow: "secondary",
  ask: "outline",
  deny: "destructive",
};

/** Clean allow/ask/deny badge — a capability-matrix cell decision. */
export function DecisionBadge({ decision }: { decision: CapabilityDecisionValue }) {
  return (
    <Badge variant={DECISION_TONE[decision]} className="capitalize">
      {decision}
    </Badge>
  );
}
