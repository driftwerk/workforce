import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { TierBadge, tierLabel } from "./tier-badge";
import {
  VAULT_TIERS,
  vaultEntryOwnerId,
  type VaultEntryView,
  type VaultTier,
} from "@/lib/api/vault";
import { useIdentities } from "@/hooks/use-identities";
import { useAuthStore } from "@/stores/auth-store";
import { ApiError } from "@/lib/api-client";
import { apiErrorMessage } from "@/lib/api-error";

const selectClass =
  "flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50";

const isPerUser = (t: VaultTier) =>
  t === "agent_per_user" || t === "global_per_user";

interface VaultMoveDialogProps {
  entry: VaultEntryView | null;
  /** Tiers the active user can move TO. T3/T4 only enabled for admins. */
  allowedDestinations: VaultTier[];
  /**
   * Called with the chosen tier and (for per-user destinations) the owner the
   * entry should belong to. Resolve on success; reject to surface an error.
   */
  onMove: (
    entry: VaultEntryView,
    to: VaultTier,
    overwrite: boolean,
    userId?: string,
  ) => Promise<void>;
  pending: boolean;
  onOpenChange: (open: boolean) => void;
}

export function VaultMoveDialog({
  entry,
  allowedDestinations,
  onMove,
  pending,
  onOpenChange,
}: VaultMoveDialogProps) {
  const [target, setTarget] = useState<VaultTier | null>(null);
  const [overwrite, setOverwrite] = useState(false);
  const [userId, setUserId] = useState("");
  const [error, setError] = useState<{ message: string; conflict: boolean } | null>(null);

  const { data: identitiesData } = useIdentities();
  const currentUser = useAuthStore((s) => s.identity);

  const open = !!entry;
  const tiersForChoice = VAULT_TIERS.filter(
    (t) => t !== entry?.tier && allowedDestinations.includes(t),
  );

  // The source's owner is fixed when moving an already per-user entry — a move
  // carries a single user scope, so it can't reassign ownership. Otherwise the
  // owner is picked (defaulting to the acting user) when the destination is
  // per-user.
  const sourceOwner =
    entry && isPerUser(entry.tier) ? vaultEntryOwnerId(entry) : null;
  const ownerLocked = sourceOwner !== null;

  // Reset the form each time the dialog opens for a new entry.
  useEffect(() => {
    if (!open) return;
    setTarget(null);
    setOverwrite(false);
    setError(null);
    setUserId(sourceOwner ?? currentUser?.id ?? "");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, entry?.path, entry?.tier]);

  const identities = identitiesData?.identities ?? [];
  const sortedIdentities = useMemo(() => {
    const list = [...identities];
    list.sort((a, b) => {
      if (a.id === currentUser?.id) return -1;
      if (b.id === currentUser?.id) return 1;
      return a.display_name.localeCompare(b.display_name);
    });
    return list;
  }, [identities, currentUser]);

  const needsOwner = target !== null && isPerUser(target);
  // Effective owner sent to the backend: fixed source owner when relocating a
  // per-user entry, otherwise the picked user for a per-user destination.
  const effectiveUserId = ownerLocked ? (sourceOwner ?? undefined) : userId;
  const ownerMissing = needsOwner && !effectiveUserId;

  async function handleConfirm() {
    if (!entry || !target || ownerMissing) return;
    setError(null);
    try {
      await onMove(entry, target, overwrite, needsOwner ? effectiveUserId : undefined);
    } catch (e) {
      setError({
        message: apiErrorMessage(e),
        conflict: e instanceof ApiError && e.status === 409,
      });
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Move secret</DialogTitle>
          <DialogDescription>
            {entry ? (
              <>
                Move <span className="font-mono">{entry.path}</span> from
                <span className="mx-2"><TierBadge tier={entry.tier} /></span>
                to a different tier. The value never appears on screen — only its
                tier changes.
              </>
            ) : null}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-2">
            <div className="text-sm font-medium">Destination tier</div>
            <div className="grid grid-cols-1 gap-2">
              {tiersForChoice.length === 0 ? (
                <div className="text-sm text-muted-foreground">
                  No other tiers are reachable for this entry.
                </div>
              ) : (
                tiersForChoice.map((t) => (
                  <button
                    key={t}
                    type="button"
                    onClick={() => setTarget(t)}
                    className={
                      "flex items-center justify-between rounded-md border px-3 py-2 text-left hover:bg-muted/40 " +
                      (target === t ? "border-foreground" : "border-muted")
                    }
                  >
                    <span className="text-sm font-medium">{tierLabel(t)}</span>
                    <TierBadge tier={t} />
                  </button>
                ))
              )}
            </div>
          </div>

          {/* Owner picker — per-user destinations are stored under users/<id>/… */}
          {needsOwner && (
            <div className="space-y-1.5">
              <label className="text-sm font-medium leading-none">User</label>
              <select
                className={selectClass}
                value={effectiveUserId ?? ""}
                disabled={ownerLocked}
                onChange={(e) => setUserId(e.target.value)}
              >
                <option value="">Select a user…</option>
                {sortedIdentities.map((u) => (
                  <option key={u.id} value={u.id}>
                    {u.display_name}
                    {u.id === currentUser?.id ? " (you)" : ""}
                  </option>
                ))}
              </select>
              <p className="text-xs text-muted-foreground">
                {ownerLocked
                  ? "Owner is kept from the source entry — a move can't reassign ownership."
                  : "The secret will be stored privately for this user."}
              </p>
            </div>
          )}

          {error && !error.conflict && (
            <div className="rounded-md border border-destructive bg-destructive/10 p-3 text-sm text-destructive">
              {error.message}
            </div>
          )}

          {error?.conflict && (
            <div className="rounded-md border border-amber-500 bg-amber-50 p-3 text-sm">
              {error.message}
              <label className="mt-2 flex items-center gap-2 text-xs">
                <input
                  type="checkbox"
                  checked={overwrite}
                  onChange={(e) => setOverwrite(e.target.checked)}
                />
                Overwrite the existing destination value
              </label>
            </div>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={handleConfirm} disabled={!target || ownerMissing || pending}>
            {pending ? "Moving…" : "Move"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
