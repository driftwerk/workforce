import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useDeleteMonitor } from "@/hooks/use-monitors";
import type { MonitorDef } from "@/lib/api/monitors";

interface DeleteMonitorDialogProps {
  monitor: MonitorDef | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDeleted?: () => void;
}

export function DeleteMonitorDialog({ monitor, open, onOpenChange, onDeleted }: DeleteMonitorDialogProps) {
  const deleteMutation = useDeleteMonitor();

  const handleDelete = () => {
    if (!monitor) return;
    deleteMutation.mutate(
      { id: monitor.id, owner: monitor.owner },
      {
        onSuccess: () => {
          onOpenChange(false);
          onDeleted?.();
        },
      },
    );
  };

  if (!monitor) return null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete Monitor</DialogTitle>
          <DialogDescription>
            Are you sure you want to delete <strong>{monitor.name}</strong>? This action cannot be undone.
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
