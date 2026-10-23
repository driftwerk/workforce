import { useEffect, useMemo, useRef, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { MultiCombobox } from "@/components/permissions/combobox";
import { useIdentities } from "@/hooks/use-identities";
import { useIdentityTeams } from "@/hooks/use-teams";
import { useDefShares, useSetDefShares } from "@/hooks/use-shares";
import type {
  DefShare,
  PrincipalType,
  ShareKind,
  SharePermission,
} from "@/lib/api/shares";

interface ShareDialogProps {
  kind: ShareKind;
  id: string;
  /** The definition's owner; sharing is owner/admin-only and addresses this row. */
  owner?: string | null;
  name: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

const KIND_NOUN: Record<ShareKind, string> = {
  workflows: "workflow",
  playbooks: "playbook",
  monitors: "monitor",
  "wasm-skills": "skill",
  "external-skills": "skill",
};

const principalKey = (type: PrincipalType, id: string) => `${type}:${id}`;

function splitKey(key: string): { type: PrincipalType; id: string } {
  const i = key.indexOf(":");
  return { type: key.slice(0, i) as PrincipalType, id: key.slice(i + 1) };
}

/**
 * Share an owned resource of any kind with specific users or teams. The
 * multiselect is the authoritative grantee set: selecting shares, deselecting
 * unshares. Each grantee is `run` (run + view) or `manage` (also edit + delete,
 * never re-share).
 */
export function ShareDialog({
  kind,
  id,
  owner,
  name,
  open,
  onOpenChange,
}: ShareDialogProps) {
  const identities = useIdentities();
  const teams = useIdentityTeams();
  const sharesQuery = useDefShares(kind, open ? id : undefined, owner);
  const current = sharesQuery.data;
  const setShares = useSetDefShares(kind);

  const [values, setValues] = useState<string[]>([]);
  const [perms, setPerms] = useState<Record<string, SharePermission>>({});
  const seeded = useRef(false);

  // Seed the selection once, when the current grants first load for an open
  // dialog. Not keyed on every `current` change: a background refetch must not
  // clobber in-progress edits, and a not-yet-loaded query must not seed an empty
  // set that a save would then persist as an unshare.
  useEffect(() => {
    if (!open) {
      seeded.current = false;
      return;
    }
    if (seeded.current || !current) return;
    seeded.current = true;
    setValues(
      current.shares.map((g) => principalKey(g.principal_type, g.principal_id)),
    );
    setPerms(
      Object.fromEntries(
        current.shares.map((g) => [
          principalKey(g.principal_type, g.principal_id),
          g.permission,
        ]),
      ),
    );
  }, [open, current]);

  const options = useMemo(() => {
    const users = (identities.data?.identities ?? [])
      .filter((u) => u.id !== owner)
      .map((u) => ({
        value: principalKey("user", u.id),
        label: u.display_name || u.email || u.id,
        sub: u.email ?? undefined,
      }));
    const teamOptions = (teams.data?.teams ?? []).map((t) => ({
      value: principalKey("team", t.id),
      label: `${t.name} (team)`,
      sub: `${t.member_count} member${t.member_count === 1 ? "" : "s"}`,
    }));
    return [...users, ...teamOptions];
  }, [identities.data, teams.data, owner]);

  const labelFor = (key: string) =>
    options.find((o) => o.value === key)?.label ?? key;

  const handleSave = () => {
    const shares: DefShare[] = values.map((key) => {
      const { type, id: principalId } = splitKey(key);
      return {
        principal_type: type,
        principal_id: principalId,
        permission: perms[key] ?? "run",
      };
    });
    setShares.mutate({ id, shares, owner }, { onSuccess: () => onOpenChange(false) });
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Share “{name}”</DialogTitle>
          <DialogDescription>
            Choose who can access this {KIND_NOUN[kind]}. <strong>Run</strong> lets
            them run and view it; <strong>Manage</strong> also lets them edit and
            delete it — never re-share it.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          {sharesQuery.isError ? (
            <p className="text-sm text-destructive">
              Couldn’t load who this is shared with. Close and try again.
            </p>
          ) : !current ? (
            <p className="text-sm text-muted-foreground">Loading…</p>
          ) : (
            <>
              <MultiCombobox
                options={options}
                values={values}
                onChange={setValues}
                placeholder="Add people or teams…"
                searchPlaceholder="Search people and teams…"
                isLoading={identities.isLoading || teams.isLoading}
                isError={identities.isError || teams.isError}
              />
              {values.length > 0 && (
                <ul className="space-y-1">
                  {values.map((key) => (
                    <li
                      key={key}
                      className="flex items-center justify-between gap-2 text-sm"
                    >
                      <span className="truncate">{labelFor(key)}</span>
                      <select
                        className="rounded-md border bg-background px-2 py-1 text-xs"
                        value={perms[key] ?? "run"}
                        onChange={(e) =>
                          setPerms((p) => ({
                            ...p,
                            [key]: e.target.value as SharePermission,
                          }))
                        }
                      >
                        <option value="run">Run</option>
                        <option value="manage">Manage</option>
                      </select>
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={handleSave} disabled={!current || setShares.isPending}>
            {setShares.isPending ? "Saving…" : "Save sharing"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
