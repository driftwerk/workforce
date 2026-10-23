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
import { useVault, useVaultDelete, useVaultCatalog } from "@/hooks/use-vault";
import { VaultTable } from "./vault-table";
import { VaultSecretForm } from "./vault-secret-form";
import { vaultEntryOwnerId, type VaultEntryView } from "@/lib/api/vault";
import { apiErrorMessage } from "@/lib/api-error";

/**
 * Admin-only global vault browser. Shows every T2 + T4 entry in the global
 * vault with owner display names. Add/rename go through the catalog-driven
 * {@link VaultSecretForm}; tier moves remain on the per-agent vault page.
 */
export function VaultBrowser() {
  const { data, isLoading } = useVault();
  const { data: catalogData } = useVaultCatalog();
  const deleteMutation = useVaultDelete();

  const [addOpen, setAddOpen] = useState(false);
  const [renameTarget, setRenameTarget] = useState<VaultEntryView | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<VaultEntryView | null>(null);

  const entries = data?.entries ?? [];

  function handleDelete() {
    if (!deleteTarget) return;
    const physicalPath =
      deleteTarget.tier === "global_per_user"
        ? `users/${vaultEntryOwnerId(deleteTarget)}/${deleteTarget.path}`
        : deleteTarget.path;
    deleteMutation.mutate(physicalPath, {
      onSuccess: () => setDeleteTarget(null),
    });
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
        context={{ kind: "global" }}
        mode="add"
      />

      <VaultSecretForm
        open={!!renameTarget}
        onOpenChange={(o) => !o && setRenameTarget(null)}
        context={{ kind: "global" }}
        mode="rename"
        entry={renameTarget}
      />

      <Dialog open={!!deleteTarget} onOpenChange={() => setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Secret</DialogTitle>
            <DialogDescription>
              Delete <span className="font-mono font-semibold">{deleteTarget?.path}</span>{" "}
              at tier <span className="font-semibold">{deleteTarget?.tier}</span>?
              This action cannot be undone.
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
