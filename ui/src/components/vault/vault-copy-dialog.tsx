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
import { TierBadge } from "./tier-badge";
import {
  vaultEntryOwnerId,
  type VaultCopyResponse,
  type VaultCopyStatus,
  type VaultEntryView,
} from "@/lib/api/vault";
import { useIdentities } from "@/hooks/use-identities";
import { useAuthStore } from "@/stores/auth-store";

interface VaultCopyDialogProps {
  /** Source per-user entry whose value is copied to other users. */
  entry: VaultEntryView | null;
  pending: boolean;
  /** Per-user outcomes from the last submit, shown inline. */
  result: VaultCopyResponse | null;
  error: string | null;
  onCopy: (toUserIds: string[], overwrite: boolean) => void;
  onOpenChange: (open: boolean) => void;
}

const STATUS_STYLE: Record<VaultCopyStatus, { label: string; cls: string }> = {
  copied: { label: "copied", cls: "text-green-600 dark:text-green-400" },
  exists: { label: "exists — not overwritten", cls: "text-amber-600 dark:text-amber-400" },
  source: { label: "source — skipped", cls: "text-muted-foreground" },
  error: { label: "error", cls: "text-destructive" },
};

export function VaultCopyDialog({
  entry,
  pending,
  result,
  error,
  onCopy,
  onOpenChange,
}: VaultCopyDialogProps) {
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [overwrite, setOverwrite] = useState(false);

  const { data: identitiesData } = useIdentities();
  const currentUser = useAuthStore((s) => s.identity);

  const open = !!entry;
  const sourceOwner = entry ? vaultEntryOwnerId(entry) : null;

  useEffect(() => {
    if (!open) return;
    setSelected(new Set());
    setOverwrite(false);
  }, [open, entry?.path, entry?.tier]);

  const identities = identitiesData?.identities ?? [];
  const candidates = useMemo(
    () =>
      identities
        .filter((u) => u.id !== sourceOwner)
        .sort((a, b) => a.display_name.localeCompare(b.display_name)),
    [identities, sourceOwner],
  );
  const nameOf = (uid: string) =>
    identities.find((u) => u.id === uid)?.display_name ?? uid;

  function toggle(uid: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(uid)) next.delete(uid);
      else next.add(uid);
      return next;
    });
  }

  function handleConfirm() {
    if (selected.size === 0) return;
    onCopy([...selected], overwrite);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Copy secret to users</DialogTitle>
          <DialogDescription>
            {entry ? (
              <>
                Copy <span className="font-mono">{entry.path}</span> owned by{" "}
                <span className="font-medium">
                  {entry.created_by_display ?? entry.created_by}
                </span>{" "}
                from
                <span className="mx-2"><TierBadge tier={entry.tier} /></span>
                into other users' private slots. The source is kept and the value
                never appears on screen.
              </>
            ) : null}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-2">
            <div className="text-sm font-medium">Grant to users</div>
            {candidates.length === 0 ? (
              <div className="text-sm text-muted-foreground">
                No other users are available.
              </div>
            ) : (
              <div className="max-h-56 space-y-1 overflow-y-auto rounded-md border p-2">
                {candidates.map((u) => (
                  <label
                    key={u.id}
                    className="flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-muted/40"
                  >
                    <input
                      type="checkbox"
                      checked={selected.has(u.id)}
                      onChange={() => toggle(u.id)}
                    />
                    {u.display_name}
                    {u.id === currentUser?.id ? " (you)" : ""}
                  </label>
                ))}
              </div>
            )}
          </div>

          <label className="flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={overwrite}
              onChange={(e) => setOverwrite(e.target.checked)}
            />
            Replace any existing values for the selected users
          </label>

          {result && (
            <div className="space-y-1 rounded-md border p-3 text-sm">
              <div className="font-medium">
                Copied to {result.copied} of {result.results.length} user(s)
              </div>
              {result.results.map((o) => {
                const s = STATUS_STYLE[o.status] ?? {
                  label: o.status,
                  cls: "text-muted-foreground",
                };
                return (
                  <div key={o.user_id} className="flex justify-between gap-2">
                    <span>{nameOf(o.user_id)}</span>
                    <span className={s.cls}>{o.error ?? s.label}</span>
                  </div>
                );
              })}
            </div>
          )}

          {error && (
            <div className="rounded-md border border-destructive bg-destructive/10 p-3 text-sm text-destructive">
              {error}
            </div>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {result ? "Close" : "Cancel"}
          </Button>
          <Button onClick={handleConfirm} disabled={selected.size === 0 || pending}>
            {pending ? "Copying…" : "Copy"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
