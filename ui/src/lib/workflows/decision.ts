import type { DecisionChoice, DecisionSpec } from "@/lib/api/workflows";

/** A decision step's badge: "decision · 1 button", "decision · 3 buttons". */
export function decisionBadge({ buttons }: DecisionSpec): string {
  return `decision · ${buttons.length} ${buttons.length === 1 ? "button" : "buttons"}`;
}

/** The button variant a decision choice's style asks for. */
export function choiceVariant(style: DecisionChoice["style"]): "default" | "destructive" | "outline" {
  switch (style) {
    case "primary":
      return "default";
    case "danger":
      return "destructive";
    default:
      return "outline";
  }
}

/** Names as one alternative: "Alice", "Alice or Bob", "Alice, Bob or Ana". */
export function waitingOnSentence(names: string[]): string {
  if (names.length === 0) return "nobody";
  if (names.length === 1) return names[0]!;
  return `${names.slice(0, -1).join(", ")} or ${names[names.length - 1]}`;
}

function inWords(ms: number): string {
  const minutes = Math.max(1, Math.round(ms / 60_000));
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/** When a decision expires and what expiry chooses, or null when it never does. */
export function expiryText(
  expiresAt: string | null,
  onExpiryLabel: string | null,
  now: Date = new Date(),
): string | null {
  if (!expiresAt) return null;
  const left = new Date(expiresAt).getTime() - now.getTime();
  const when = left > 0 ? `Expires in ${inWords(left)}` : "Expiring now";
  return onExpiryLabel ? `${when} → ${onExpiryLabel}` : when;
}
