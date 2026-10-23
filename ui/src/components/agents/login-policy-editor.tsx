import { ArrowDown, ArrowUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { LoginPolicyView } from "@/lib/api/subscriptions";
import {
  DEFAULT_POLICY,
  SHARED_SERVES,
  canExclude,
  moveTier,
  readPolicy,
  seedOverride,
  tierRows,
  toggleTier,
  type LoginPolicy,
} from "@/lib/subscriptions/login-policy";
import { SERVES_LABELS, TIER_LABELS } from "@/lib/subscriptions/status";

/** `[llm.logins]`: where the agent looks for a subscription login, and whom
 *  its shared login serves. The block replaces the inherited policy whole,
 *  so overriding writes both keys. */
export function LoginPolicyEditor({
  block,
  effective,
  disabled,
  onChange,
}: {
  block: unknown;
  effective: LoginPolicyView | undefined;
  disabled: boolean;
  onChange: (policy: LoginPolicy | undefined) => void;
}) {
  const policy = readPolicy(block);

  if (!policy) {
    const inherited = effective && !effective.overridden ? effective : null;
    return (
      <div className="space-y-2">
        {inherited ? (
          <div className="text-xs text-muted-foreground space-y-1">
            <p>Inherited: tried in this order —</p>
            <ol className="list-decimal pl-5">
              {inherited.tiers.map((tier) => (
                <li key={tier}>{TIER_LABELS[tier]}</li>
              ))}
            </ol>
            <p>Shared logins serve: {SERVES_LABELS[inherited.shared_serves]}</p>
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">The inherited policy applies once saved.</p>
        )}
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled}
          onClick={() => onChange(seedOverride(effective ?? DEFAULT_POLICY))}
        >
          Override for this agent
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <ul className="space-y-1">
        {tierRows(policy).map(({ tier, position }) => {
          const included = position !== null;
          return (
            <li key={tier} className="flex items-center gap-2 text-xs">
              <input
                type="checkbox"
                id={`login-tier-${tier}`}
                checked={included}
                disabled={disabled || (included && !canExclude(policy, tier))}
                onChange={() => onChange(toggleTier(policy, tier))}
              />
              <label htmlFor={`login-tier-${tier}`} className="flex-1">
                {included && <span className="text-muted-foreground mr-1">{position + 1}.</span>}
                {TIER_LABELS[tier]}
                <span className="ml-1 font-mono text-muted-foreground">({tier})</span>
              </label>
              {included && (
                <>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 w-6 p-0"
                    aria-label={`Try ${TIER_LABELS[tier].toLowerCase()} earlier`}
                    disabled={disabled || position === 0}
                    onClick={() => onChange(moveTier(policy, tier, -1))}
                  >
                    <ArrowUp className="h-3 w-3" />
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 w-6 p-0"
                    aria-label={`Try ${TIER_LABELS[tier].toLowerCase()} later`}
                    disabled={disabled || position === policy.tiers.length - 1}
                    onClick={() => onChange(moveTier(policy, tier, 1))}
                  >
                    <ArrowDown className="h-3 w-3" />
                  </Button>
                </>
              )}
            </li>
          );
        })}
      </ul>
      <div className="space-y-1">
        <label htmlFor="login-shared-serves" className="text-xs font-medium text-muted-foreground">
          Shared logins serve
        </label>
        <select
          id="login-shared-serves"
          className="h-8 w-full rounded-md border border-input bg-background px-2 text-sm"
          value={policy.shared_serves}
          disabled={disabled}
          onChange={(e) =>
            onChange({ ...policy, shared_serves: e.target.value as LoginPolicy["shared_serves"] })
          }
        >
          {SHARED_SERVES.map((serves) => (
            <option key={serves} value={serves}>
              {serves} — {SERVES_LABELS[serves]}
            </option>
          ))}
        </select>
      </div>
      <Button type="button" variant="outline" size="sm" disabled={disabled} onClick={() => onChange(undefined)}>
        Use inherited policy
      </Button>
    </div>
  );
}
