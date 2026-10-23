import { useState } from "react";
import { Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  useAgentVaultKeys,
  useAgentVaultDelete,
  useAgentVaultMove,
  useAgentVaultCopy,
} from "@/hooks/use-agents";
import { useVaultCatalog } from "@/hooks/use-vault";
import { VaultTable } from "@/components/vault/vault-table";
import { VaultMoveDialog } from "@/components/vault/vault-move-dialog";
import { VaultCopyDialog } from "@/components/vault/vault-copy-dialog";
import { VaultSecretForm } from "@/components/vault/vault-secret-form";
import type {
  VaultCopyResponse,
  VaultEntryView,
  VaultTier,
} from "@/lib/api/vault";
import { VAULT_TIERS, vaultEntryOwnerId } from "@/lib/api/vault";
import { useAuthStore } from "@/stores/auth-store";
import { apiErrorMessage } from "@/lib/api-error";

interface AgentVaultProps {
  agentId: string;
}

const ALL_DESTINATIONS: VaultTier[] = [...VAULT_TIERS];

const isPerUser = (t: VaultTier) =>
  t === "agent_per_user" || t === "global_per_user";

export function AgentVault({ agentId }: AgentVaultProps) {
  const { data, isLoading } = useAgentVaultKeys(agentId);
  const { data: catalogData } = useVaultCatalog();
  const deleteMutation = useAgentVaultDelete(agentId);
  const moveMutation = useAgentVaultMove(agentId);
  const copyMutation = useAgentVaultCopy(agentId);
  const isSystemAdmin = useAuthStore(
    (s) => s.identity?.role === "system_admin",
  );

  const [addOpen, setAddOpen] = useState(false);
  const [renameTarget, setRenameTarget] = useState<VaultEntryView | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<VaultEntryView | null>(null);
  const [moveTarget, setMoveTarget] = useState<VaultEntryView | null>(null);
  const [copyTarget, setCopyTarget] = useState<VaultEntryView | null>(null);
  const [copyResult, setCopyResult] = useState<VaultCopyResponse | null>(null);
  const [copyError, setCopyError] = useState<string | null>(null);

  const entries: VaultEntryView[] = data?.entries ?? [];

  function handleDelete() {
    if (!deleteTarget) return;
    const physical =
      deleteTarget.tier === "agent_per_user"
        ? `users/${vaultEntryOwnerId(deleteTarget)}/${deleteTarget.path}`
        : deleteTarget.path;
    deleteMutation.mutate(physical, { onSuccess: () => setDeleteTarget(null) });
  }

  async function handleMove(
    entry: VaultEntryView,
    to: VaultTier,
    overwrite: boolean,
    userId?: string,
  ) {
    // Relocating an already per-user entry keeps its owner; moving a shared
    // entry into a per-user tier takes the owner the dialog picked.
    const user_id =
      entry.tier === "agent_per_user" || entry.tier === "global_per_user"
        ? vaultEntryOwnerId(entry)
        : userId;
    await moveMutation.mutateAsync({
      path: entry.path,
      from_tier: entry.tier,
      to_tier: to,
      user_id,
      overwrite,
    });
    setMoveTarget(null);
  }

  function handleCopy(toUserIds: string[], overwrite: boolean) {
    if (!copyTarget) return;
    setCopyError(null);
    setCopyResult(null);
    copyMutation.mutate(
      {
        path: copyTarget.path,
        from_tier: copyTarget.tier,
        to_tier: copyTarget.tier,
        user_id: vaultEntryOwnerId(copyTarget),
        to_user_ids: toUserIds,
        overwrite,
      },
      {
        onSuccess: (data) => setCopyResult(data),
        onError: (e) => setCopyError(apiErrorMessage(e)),
      },
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-end">
        <Button onClick={() => setAddOpen(true)}>
          <Plus className="h-4 w-4 mr-1" />
          Add Secret
        </Button>
      </div>

      <VaultTable
        entries={entries}
        isLoading={isLoading}
        showOwner
        catalog={catalogData?.entries}
        canRename={() => true}
        onRename={setRenameTarget}
        canMove={() => true}
        onMove={setMoveTarget}
        canCopy={(e) => isSystemAdmin && isPerUser(e.tier)}
        onCopy={(e) => {
          setCopyResult(null);
          setCopyError(null);
          setCopyTarget(e);
        }}
        canDelete={() => true}
        onDelete={(e) => {
          deleteMutation.reset();
          setDeleteTarget(e);
        }}
        emptyAction={{ label: "Add Secret", onClick: () => setAddOpen(true) }}
      />

      <VaultSecretForm
        open={addOpen}
        onOpenChange={setAddOpen}
        context={{ kind: "agent", agentId }}
        mode="add"
      />

      <VaultSecretForm
        open={!!renameTarget}
        onOpenChange={(o) => !o && setRenameTarget(null)}
        context={{ kind: "agent", agentId }}
        mode="rename"
        entry={renameTarget}
      />

      <VaultMoveDialog
        entry={moveTarget}
        allowedDestinations={ALL_DESTINATIONS}
        pending={moveMutation.isPending}
        onOpenChange={(o) => !o && setMoveTarget(null)}
        onMove={handleMove}
      />

      <VaultCopyDialog
        entry={copyTarget}
        pending={copyMutation.isPending}
        result={copyResult}
        error={copyError}
        onCopy={handleCopy}
        onOpenChange={(o) => {
          if (!o) {
            setCopyTarget(null);
            setCopyResult(null);
            setCopyError(null);
          }
        }}
      />

      <Dialog open={!!deleteTarget} onOpenChange={() => setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Secret</DialogTitle>
            <DialogDescription>
              Delete <span className="font-mono font-semibold">{deleteTarget?.path}</span>{" "}
              at tier <span className="font-semibold">{deleteTarget?.tier}</span>?
            </DialogDescription>
          </DialogHeader>
          {deleteMutation.error && (
            <p className="text-xs text-destructive">
              {apiErrorMessage(deleteMutation.error)}
            </p>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteTarget(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteMutation.isPending}
              onClick={handleDelete}
            >
              {deleteMutation.isPending ? "Deleting..." : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
