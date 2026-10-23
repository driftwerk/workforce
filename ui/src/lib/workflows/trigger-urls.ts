import type { TriggerUrl } from "../api/workflows";

/**
 * What to show for one HTTP trigger.
 *
 * `primary` is the address to hand to whoever will call the workflow: the
 * minted one where there is one, otherwise the owner-derived one, which is all
 * a hub without a definition store ever has. `alsoAnswers` is the older
 * address, and is filled **only** when both forms answer — a trigger whose
 * owner-derived path was dropped for a collision, or which has no minted one,
 * must never be labelled as answering on an address that does not.
 */
export interface TriggerAddresses {
  triggerId: string;
  primary: string | null;
  alsoAnswers: string | null;
}

export function triggerAddresses(trigger: TriggerUrl): TriggerAddresses {
  const { trigger_id: triggerId, url, legacy_url: legacy } = trigger;
  return {
    triggerId,
    primary: url ?? legacy ?? null,
    alsoAnswers: url && legacy ? legacy : null,
  };
}

/** Every trigger's addresses, in declaration order. */
export function triggerAddressList(triggers: TriggerUrl[] | undefined): TriggerAddresses[] {
  return (triggers ?? []).map(triggerAddresses);
}
