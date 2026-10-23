import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useDeletePlaybook } from "@/hooks/use-playbooks";
import type { PlaybookDef } from "@/lib/api/playbooks";

interface DeletePlaybookDialogProps {
  playbook: PlaybookDef | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDeleted?: () => void;
}

export function DeletePlaybookDialog({ playbook, open, onOpenChange, onDeleted }: DeletePlaybookDialogProps) {
  const deleteMutation = useDeletePlaybook();

  const handleDelete = () => {
    if (!playbook) return;
    deleteMutation.mutate(
      { id: playbook.id, owner: playbook.owner },
      {
        onSuccess: () => {
          onOpenChange(false);
          onDeleted?.();
        },
      },
    );
  };

  if (!playbook) return null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete Playbook</DialogTitle>
          <DialogDescription>
            Are you sure you want to delete <strong>{playbook.name}</strong>? This action cannot be undone.
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button variant="destructive" onClick={handleDelete} disabled={deleteMutation.isPending}>
            {deleteMutation.isPending ? "Deleting…" : "Delete"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
