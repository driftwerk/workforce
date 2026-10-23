import type { ReactNode } from "react";
import { Link } from "react-router";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { InlineCode } from "@/components/ui/inline-code";
import { Skeleton } from "@/components/ui/skeleton";
import { LoginStatusBadge } from "@/components/subscriptions/login-status-badge";
import { LoginConnect } from "@/components/subscriptions/login-connect";
import { useAgentLogins, useMySubscriptions } from "@/hooks/use-subscriptions";
import type {
  AgentLoginsView,
  LoginSlotView,
  LoginStatus,
  ServingView,
  SubscriptionKind,
} from "@/lib/api/subscriptions";
import { apiErrorMessage } from "@/lib/api-error";
import { formatDateTime } from "@/lib/utils";
import {
  CHATGPT_DIRECT_OFF,
  HOLDER_LABELS,
  KIND_LABELS,
  ownLoginStatus,
  SERVES_LABELS,
  TIER_LABELS,
} from "@/lib/subscriptions/status";

const PER_SERVER = "Limits and sign-outs are as seen by this server.";

function ServingRow({ serving }: { serving: ServingView }) {
  const kind = KIND_LABELS[serving.kind];
  return (
    <li className="space-y-1">
      {serving.tier ? (
        <p className="text-sm">
          <span className="font-medium">{kind}</span>: {TIER_LABELS[serving.tier]}
        </p>
      ) : (
        <>
          <p className="text-sm">
            <span className="font-medium">{kind}</span>: no login serves you.{" "}
            <Link to={`/my-settings#${serving.kind}`} className="underline">
              Connect your own
            </Link>
          </p>
          <ul className="pl-4 text-xs text-muted-foreground list-disc">
            {serving.misses.map((miss) => (
              <li key={miss.tier}>
                {TIER_LABELS[miss.tier]}: {miss.reason}
              </li>
            ))}
          </ul>
        </>
      )}
    </li>
  );
}

/** One login's row: what it is, where it stands, and what may be done. */
function LoginRow({
  title,
  detail,
  status,
  children,
}: {
  title: ReactNode;
  detail?: ReactNode;
  status?: LoginStatus;
  children: ReactNode;
}) {
  return (
    <div className="space-y-3 rounded-md border p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium">{title}</p>
          {detail}
        </div>
        {status && <LoginStatusBadge status={status} />}
      </div>
      {children}
    </div>
  );
}

function SharedLogin({
  agentId,
  slot,
  canManage,
}: {
  agentId: string;
  slot: LoginSlotView;
  canManage: boolean;
}) {
  return (
    <LoginRow
      title={`${KIND_LABELS[slot.kind]} · ${TIER_LABELS[slot.tier]}`}
      detail={
        slot.updated_at && (
          <p className="text-xs text-muted-foreground">
            Set {formatDateTime(slot.updated_at)}
            {slot.updated_by && ` by ${slot.updated_by}`}
          </p>
        )
      }
      status={slot.status}
    >
      {canManage ? (
        <LoginConnect
          kind={slot.kind}
          target={slot.tier === "agent_shared" ? { tier: slot.tier, agent_id: agentId } : { tier: slot.tier }}
          connected={slot.status.state !== "not_set"}
        />
      ) : (
        <p className="text-xs text-muted-foreground">
          Ask an admin or the agent owner {slot.status.state === "not_set" ? "to set a shared login" : "to change it"}.
        </p>
      )}
    </LoginRow>
  );
}

function OwnLogins({ agentId, kinds }: { agentId: string; kinds: SubscriptionKind[] }) {
  const mine = useMySubscriptions();
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{TIER_LABELS.agent_user}</CardTitle>
        <CardDescription>Used for your messages on this agent instead of your login on every agent.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {mine.error ? (
          <p className="text-sm text-destructive">Failed to load your logins: {apiErrorMessage(mine.error)}</p>
        ) : (
          kinds.map((kind) => {
            const status = mine.data && ownLoginStatus(mine.data.logins, kind, "agent_user", agentId);
            return (
              <LoginRow key={kind} title={KIND_LABELS[kind]} status={status}>
                {status ? (
                  <LoginConnect
                    kind={kind}
                    target={{ tier: "agent_user", agent_id: agentId }}
                    connected={status.state !== "not_set"}
                  />
                ) : (
                  <Skeleton className="h-8 w-40" />
                )}
              </LoginRow>
            );
          })
        )}
      </CardContent>
    </Card>
  );
}

function LoginsView({ agentId, view }: { agentId: string; view: AgentLoginsView }) {
  const canManage = (slot: LoginSlotView) =>
    slot.tier === "system_shared" ? view.can_manage_install : view.can_manage_shared;

  if (view.kinds.length === 0) {
    return (
      <p className="text-sm text-muted-foreground py-8 text-center">
        None of this agent's models runs on a Claude or ChatGPT plan.
      </p>
    );
  }

  return (
    <div className="space-y-4">
      {!view.install.chatgpt_direct && view.kinds.includes("chatgpt") && (
        <p role="status" className="text-sm text-warning">
          {CHATGPT_DIRECT_OFF}
        </p>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Which login serves you</CardTitle>
          <CardDescription>The login this agent would use for your messages right now. {PER_SERVER}</CardDescription>
        </CardHeader>
        <CardContent>
          <ul className="space-y-2">
            {view.serving_you.map((serving) => (
              <ServingRow key={serving.kind} serving={serving} />
            ))}
          </ul>
        </CardContent>
      </Card>

      {view.policy.tiers.includes("agent_user") && <OwnLogins agentId={agentId} kinds={view.kinds} />}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Shared logins</CardTitle>
          <CardDescription>
            Logins everyone on this agent may run on, as the policy allows. Shared logins serve:{" "}
            {SERVES_LABELS[view.policy.shared_serves].toLowerCase()}.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {view.shared.map((slot) => (
            <SharedLogin key={`${slot.kind}-${slot.tier}`} agentId={agentId} slot={slot} canManage={canManage(slot)} />
          ))}
        </CardContent>
      </Card>

      {view.users && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">People with their own login</CardTitle>
          </CardHeader>
          <CardContent>
            {view.users.length === 0 ? (
              <p className="text-sm text-muted-foreground">Nobody has stored their own login yet.</p>
            ) : (
              <ul className="space-y-1">
                {view.users.map((user) => (
                  <li key={`${user.kind}-${user.tier}-${user.display_name}`} className="flex items-center gap-2 text-sm">
                    <span>{user.display_name}</span>
                    <Badge variant="secondary">{KIND_LABELS[user.kind]}</Badge>
                    <span className="text-xs text-muted-foreground">{HOLDER_LABELS[user.tier]}</span>
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Login policy</CardTitle>
          <CardDescription>
            {view.policy.overridden ? "Set for this agent" : "Inherited"}; change it under{" "}
            <InlineCode>[llm.logins]</InlineCode> in the{" "}
            <Link to="?tab=config" className="underline">
              Configuration tab
            </Link>
            .
          </CardDescription>
        </CardHeader>
        <CardContent>
          <ol className="list-decimal pl-5 text-sm">
            {view.policy.tiers.map((tier) => (
              <li key={tier}>{TIER_LABELS[tier]}</li>
            ))}
          </ol>
        </CardContent>
      </Card>
    </div>
  );
}

export function AgentLogins({ agentId }: { agentId: string }) {
  const { data, isLoading, error } = useAgentLogins(agentId);
  if (isLoading) return <div className="text-sm text-muted-foreground py-8 text-center">Loading logins...</div>;
  if (error || !data) {
    return <div className="text-sm text-destructive py-8 text-center">Failed to load logins: {apiErrorMessage(error)}</div>;
  }
  return <LoginsView agentId={agentId} view={data} />;
}
