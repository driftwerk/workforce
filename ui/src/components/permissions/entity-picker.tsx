import { useMemo } from "react";
import { Combobox, MultiCombobox, type ComboboxOption } from "./combobox";
import { useIdentities } from "@/hooks/use-identities";
import { useAgents } from "@/hooks/use-agents";
import { useIdentityTeams } from "@/hooks/use-teams";
import type { PermissionScope } from "@/lib/api/permissions";

export type EntityKind = PermissionScope;

const KIND_NOUN: Record<EntityKind, string> = {
  user: "person",
  agent: "agent",
  team: "team",
};

/**
 * Map one entity kind to combobox options. All three lists are fetched
 * (react-query dedupes across components), so the same data also backs
 * {@link useScopeLabeler}.
 */
function useEntityOptions(kind: EntityKind): {
  options: ComboboxOption[];
  isLoading: boolean;
  isError: boolean;
} {
  const identities = useIdentities();
  const agents = useAgents();
  const teams = useIdentityTeams();

  return useMemo(() => {
    if (kind === "user") {
      return {
        isLoading: identities.isLoading,
        isError: identities.isError,
        options: (identities.data?.identities ?? []).map((i) => ({
          value: i.id,
          label: i.display_name || i.email || i.id,
          sub: i.email ?? undefined,
        })),
      };
    }
    if (kind === "agent") {
      return {
        isLoading: agents.isLoading,
        isError: agents.isError,
        options: (agents.data?.agents ?? []).map((a) => ({
          value: a.id,
          label: a.name || a.id,
          sub: a.name ? a.id : (a.model ?? undefined),
        })),
      };
    }
    return {
      isLoading: teams.isLoading,
      isError: teams.isError,
      options: (teams.data?.teams ?? []).map((t) => ({
        value: t.id,
        label: t.name,
        sub: `${t.member_count} member${t.member_count === 1 ? "" : "s"}`,
      })),
    };
  }, [
    kind,
    identities.data,
    identities.isLoading,
    identities.isError,
    agents.data,
    agents.isLoading,
    agents.isError,
    teams.data,
    teams.isLoading,
    teams.isError,
  ]);
}

/** Searchable single-select picker for a person / agent / team, by name. */
export function EntityCombobox({
  kind,
  value,
  onChange,
  id,
  surface,
}: {
  kind: EntityKind;
  value: string;
  onChange: (id: string, label: string) => void;
  id?: string;
  surface?: "dialog" | "inline";
}) {
  const { options, isLoading, isError } = useEntityOptions(kind);
  const noun = KIND_NOUN[kind];
  return (
    <Combobox
      options={options}
      value={value}
      onChange={(v, o) => onChange(v, o.label)}
      isLoading={isLoading}
      isError={isError}
      errorMessage={`Couldn't load ${noun} list.`}
      placeholder={`Select ${noun}…`}
      searchPlaceholder={`Search ${noun} by name…`}
      emptyLabel={`No ${noun} found.`}
      surface={surface}
      id={id}
    />
  );
}

/** Multi-select variant — e.g. "the teams a user belongs to". */
export function EntityMultiCombobox({
  kind,
  values,
  onChange,
  id,
  surface,
}: {
  kind: EntityKind;
  values: string[];
  onChange: (ids: string[]) => void;
  id?: string;
  surface?: "dialog" | "inline";
}) {
  const { options, isLoading, isError } = useEntityOptions(kind);
  const noun = KIND_NOUN[kind];
  return (
    <MultiCombobox
      options={options}
      values={values}
      onChange={onChange}
      isLoading={isLoading}
      isError={isError}
      errorMessage={`Couldn't load ${noun} list.`}
      placeholder={`Select ${noun}s…`}
      searchPlaceholder={`Search ${noun}s by name…`}
      emptyLabel={`No ${noun} found.`}
      surface={surface}
      id={id}
    />
  );
}

export interface ScopeLabel {
  label: string;
  sub: string | null;
}

/**
 * Resolver that turns a stored `(scope, scope_id)` back into a human label for
 * tables. Falls back to the raw id when the subject isn't loaded (deleted user,
 * cross-workspace id).
 */
export function useScopeLabeler(): (scope: PermissionScope, scopeId: string) => ScopeLabel {
  const identities = useIdentities();
  const agents = useAgents();
  const teams = useIdentityTeams();

  return useMemo(() => {
    const userMap = new Map((identities.data?.identities ?? []).map((i) => [i.id, i]));
    const agentMap = new Map((agents.data?.agents ?? []).map((a) => [a.id, a]));
    const teamMap = new Map((teams.data?.teams ?? []).map((t) => [t.id, t]));

    return (scope: PermissionScope, scopeId: string): ScopeLabel => {
      if (scope === "user") {
        const i = userMap.get(scopeId);
        if (i) return { label: i.display_name || i.email || scopeId, sub: i.email ?? null };
      } else if (scope === "agent") {
        const a = agentMap.get(scopeId);
        if (a) return { label: a.name || scopeId, sub: a.name ? a.id : null };
      } else if (scope === "team") {
        const t = teamMap.get(scopeId);
        if (t) return { label: t.name, sub: null };
      }
      return { label: scopeId, sub: null };
    };
  }, [identities.data, agents.data, teams.data]);
}
