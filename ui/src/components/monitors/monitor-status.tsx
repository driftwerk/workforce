import { Badge } from "@/components/ui/badge";
import type { MonitorBlock, MonitorDef, MonitorPoll, MonitorStatus } from "@/lib/api/monitors";

type BadgeVariant = "secondary" | "outline" | "warning" | "destructive";

const STATUS_BADGE: Record<MonitorStatus, { label: string; variant: BadgeVariant }> = {
  active: { label: "On", variant: "secondary" },
  paused: { label: "Paused", variant: "outline" },
  awaiting_invite: { label: "Awaiting invite", variant: "warning" },
  awaiting_owner: { label: "Awaiting owner", variant: "warning" },
  unknown_agent: { label: "Unknown agent", variant: "destructive" },
  failing: { label: "Failing", variant: "destructive" },
};

export function MonitorStatusBadge({ status, className }: { status: MonitorStatus; className?: string }) {
  const { label, variant } = STATUS_BADGE[status];
  return (
    <Badge variant={variant} className={className}>
      {label}
    </Badge>
  );
}

/** What the owner has to do about a block, as one sentence. */
export function blockHint(block: MonitorBlock): string {
  return `@${block.agent} is not a member of ${block.channel} — invite it with /invite @${block.agent}; watching starts within two minutes.`;
}

/** What a failed poll means for the owner, given whether the monitor still runs. */
export function pollHint(poll: MonitorPoll, status: MonitorStatus): string | null {
  if (!poll.error) return null;
  const streak = poll.failures > 1 ? ` (${poll.failures} in a row)` : "";
  if (status === "failing") return `Last poll failed${streak}: ${poll.error} — retried on its cadence.`;
  if (status === "paused" && poll.failures > 0) {
    return `Stopped after ${poll.failures} failed poll${poll.failures === 1 ? "" : "s"}: ${poll.error} — fix the source and resume.`;
  }
  return null;
}

/** The one thing standing between this monitor and watching, if anything. */
export function troubleHint(monitor: Pick<MonitorDef, "status" | "block" | "poll">): string | null {
  if (monitor.block) return blockHint(monitor.block);
  if (monitor.poll) return pollHint(monitor.poll, monitor.status);
  return null;
}
