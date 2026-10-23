import { useEffect, useMemo, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/permissions/combobox";
import { useIdentities } from "@/hooks/use-identities";
import { useAuthStore } from "@/stores/auth-store";
import { useTransferOwner } from "@/hooks/use-shares";
import { apiErrorMessage } from "@/lib/api-error";
import type { ShareKind, TransferResult } from "@/lib/api/shares";

interface TransferDialogProps {
  kind: ShareKind;
  id: string;
  /** The row's current owner — what a definition is addressed by. */
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

const IS_DEFINITION: Record<ShareKind, boolean> = {
  workflows: true,
  playbooks: true,
  monitors: true,
  "wasm-skills": false,
  "external-skills": false,
};

/** A refusal the hub words for a human reaches the human; a status it sends
 *  bare still has to say something. */
const REFUSED = "The transfer didn’t go through. Close this and try again.";

/**
 * Hand an owned resource to another member.
 *
 * Confirmed rather than saved, because it cannot be undone by the person
 * doing it: once it lands they have no standing left to reverse it. Keeping
 * manage access is offered and off by default — a transfer that quietly left
 * the previous owner a grant would not be a transfer — and the choice travels
 * *in* the transfer, which writes the grant in the same breath as the
 * handover. Sharing first is not an option: while they still own it they
 * cannot be their own grantee, and the route would refuse it.
 */
export function TransferDialog({
  kind,
  id,
  owner,
  name,
  open,
  onOpenChange,
}: TransferDialogProps) {
  const identities = useIdentities();
  const transfer = useTransferOwner(kind);
  // An administrator reaching past an owner keeps nothing themselves: the
  // grant is the outgoing owner's, so the wording has to name whose it is.
  const mine = useAuthStore((s) => s.identity)?.id === owner;

  const [recipient, setRecipient] = useState("");
  const [keepAccess, setKeepAccess] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [done, setDone] = useState<TransferResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) return;
    setRecipient("");
    setKeepAccess(false);
    setConfirming(false);
    setDone(null);
    setError(null);
  }, [open]);

  const options = useMemo(
    () =>
      (identities.data?.identities ?? [])
        .filter((u) => u.id !== owner && u.status === "active")
        .map((u) => ({
          value: u.id,
          label: u.display_name || u.email || u.id,
          sub: u.email ?? undefined,
        })),
    [identities.data, owner],
  );

  const chosen = options.find((o) => o.value === recipient);
  const recipientLabel = chosen?.label;
  // Two members can share a display name, so the line that asks for the
  // confirmation names the address as well where there is one.
  const recipientNamed = chosen?.sub
    ? `${chosen.label} (${chosen.sub})`
    : recipientLabel;
  const noun = KIND_NOUN[kind];
  const rowOwner = IS_DEFINITION[kind] ? owner : null;
  const holder = mine ? "you" : "its current owner";
  const consequence = keepAccess
    ? `${holder} will keep manage access, but not the say over who reaches it`
    : `${holder} will not be able to edit it or change who reaches it`;

  const handleTransfer = async () => {
    if (!recipient) return;
    setError(null);
    try {
      setDone(
        await transfer.mutateAsync({
          id,
          owner: recipient,
          from: rowOwner,
          keepManage: keepAccess,
        }),
      );
    } catch (e) {
      setError(apiErrorMessage(e) || REFUSED);
      setConfirming(false);
    }
  };

  const pending = transfer.isPending;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Transfer “{name}”</DialogTitle>
          <DialogDescription>
            {done
              ? `This ${noun} belongs to ${done.to_display} now.`
              : `Give this ${noun} to another member. They will own it; ${consequence}.`}
          </DialogDescription>
        </DialogHeader>

        {done ? (
          <ul role="status" className="space-y-2 text-sm text-muted-foreground">
            {done.notes.map((note) => (
              <li key={note}>{note}</li>
            ))}
          </ul>
        ) : (
          <div className="space-y-3">
            <Combobox
              options={options}
              value={recipient}
              onChange={(next) => {
                setRecipient(next);
                setConfirming(false);
              }}
              placeholder="Choose a member…"
              searchPlaceholder="Search people…"
              emptyLabel="No other active members."
              isLoading={identities.isLoading}
              isError={identities.isError}
            />
            <label className="flex items-start gap-2 text-sm">
              <input
                type="checkbox"
                className="mt-0.5"
                checked={keepAccess}
                onChange={(e) => {
                  setKeepAccess(e.target.checked);
                  setConfirming(false);
                }}
              />
              <span>
                {mine
                  ? "Keep manage access for yourself. You can still edit it afterwards — but not re-share it, and not take it back."
                  : "Leave its current owner manage access. They can still edit it afterwards — but not re-share it, and not take it back."}
              </span>
            </label>
            {IS_DEFINITION[kind] && (
              <p className="text-sm text-muted-foreground">
                Automated runs will pause until {recipientLabel ?? "the new owner"}{" "}
                saves it.{" "}
                {/* Only a workflow is addressed by an HTTP trigger, so only a
                    workflow has an address that a transfer leaves behind. */}
                {kind === "workflows" &&
                  "A trigger’s stable address does not change, but anything still calling the older address that names you will stop working."}
              </p>
            )}
            {confirming && recipient && (
              <p role="alert" className="text-sm font-medium text-destructive">
                Give “{name}” to {recipientNamed}? This cannot be undone from
                here — only they can give it back.
              </p>
            )}
            {error && (
              <p role="alert" className="text-sm text-destructive">
                {error}
              </p>
            )}
          </div>
        )}

        <DialogFooter>
          {done ? (
            <Button autoFocus onClick={() => onOpenChange(false)}>
              Close
            </Button>
          ) : (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                variant={confirming ? "destructive" : "default"}
                disabled={!recipient || pending}
                onClick={() =>
                  confirming ? handleTransfer() : setConfirming(true)
                }
              >
                {pending
                  ? "Transferring…"
                  : confirming
                    ? "Yes, transfer it"
                    : "Transfer"}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
