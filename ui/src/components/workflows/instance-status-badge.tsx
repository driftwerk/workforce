import { Badge, type BadgeProps } from "@/components/ui/badge";

const STATUS: Record<string, { label: string; variant: NonNullable<BadgeProps["variant"]> }> = {
  queued: { label: "Queued", variant: "secondary" },
  running: { label: "Running", variant: "default" },
  paused: { label: "Paused", variant: "warning" },
  completed: { label: "Completed", variant: "success" },
  failed: { label: "Failed", variant: "destructive" },
  cancelled: { label: "Cancelled", variant: "secondary" },
  exited: { label: "Exited", variant: "secondary" },
};

export function InstanceStatusBadge({ status, className }: { status: string; className?: string }) {
  const { label, variant } = STATUS[status] ?? { label: status, variant: "outline" };
  return (
    <Badge variant={variant} className={className}>
      {label}
    </Badge>
  );
}
