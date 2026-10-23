import { Badge } from "@/components/ui/badge";
import type { LoginStatus } from "@/lib/api/subscriptions";
import { loginStatusLabel, loginStatusVariant } from "@/lib/subscriptions/status";

const PER_REPLICA = "As seen by this server: another server of this hub may see it differently";

export function LoginStatusBadge({ status }: { status: LoginStatus }) {
  const perReplica = status.state === "cooling" || status.state === "invalid";
  return (
    <Badge
      variant={loginStatusVariant(status)}
      title={status.state === "invalid" ? `${status.reason}. ${PER_REPLICA}` : perReplica ? PER_REPLICA : undefined}
    >
      {loginStatusLabel(status, new Date())}
    </Badge>
  );
}
