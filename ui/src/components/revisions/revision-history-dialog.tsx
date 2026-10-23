import { useEffect, useState } from "react";
import { History, RotateCcw, Trash2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { WorkflowDiff } from "@/components/workflows/workflow-diff";
import type { DefRevision } from "@/lib/api/revisions";

/** Channel badge for a revision's edited_via. */
function ViaBadge({ via }: { via: string }) {
  const variant =
    via === "restore" ? "default" : via === "oob" ? "warning" : "secondary";
  return (
    <Badge variant={variant} className="text-[9px] h-4 px-1">
      {via}
    </Badge>
  );
}

interface RevisionHistoryDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Dialog heading, e.g. the workflow id or identity filename. */
  title: string;
  revisions: DefRevision[] | undefined;
  isLoading: boolean;
  /** What's in the editor right now — the diff's right-hand side. */
  currentContent: string;
  /** Monaco language for the diff ("toml" | "markdown"). */
  language?: string;
  /** Fetch one revision's full content (the diff's left-hand side). */
  getContent: (rev: number) => Promise<string>;
  /** Restore the selected revision (the backend records a NEW revision). */
  onRestore: (rev: number) => void;
  restorePending?: boolean;
}

/**
 * Revision history panel shared by the workflow, playbook, and identity-file
 * editors: revision list (who/when/via) on the left, a side-by-side diff of
 * the selected revision against the current content on the right, and a
 * Restore action. Restoring never rewrites history — the backend appends a
 * new revision whose content equals the chosen one.
 */
export function RevisionHistoryDialog({
  open,
  onOpenChange,
  title,
  revisions,
  isLoading,
  currentContent,
  language = "toml",
  getContent,
  onRestore,
  restorePending,
}: RevisionHistoryDialogProps) {
  const [selectedRev, setSelectedRev] = useState<number | null>(null);
  const [selectedContent, setSelectedContent] = useState<string | null>(null);

  // Reset the selection whenever the dialog opens on (possibly) new data.
  useEffect(() => {
    if (open) {
      setSelectedRev(null);
      setSelectedContent(null);
    }
  }, [open, title]);

  useEffect(() => {
    if (selectedRev === null) return;
    let cancelled = false;
    setSelectedContent(null);
    getContent(selectedRev)
      .then((c) => {
        if (!cancelled) setSelectedContent(c);
      })
      .catch(() => {
        if (!cancelled) setSelectedContent("");
      });
    return () => {
      cancelled = true;
    };
  }, [selectedRev]); // eslint-disable-line react-hooks/exhaustive-deps

  const selected = revisions?.find((r) => r.rev === selectedRev) ?? null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-6xl w-[92vw] h-[80vh] flex flex-col p-0 gap-0">
        <DialogHeader className="px-4 py-3 border-b shrink-0">
          <DialogTitle className="text-sm flex items-center gap-2">
            <History className="h-4 w-4" />
            History — {title}
          </DialogTitle>
          <DialogDescription className="text-xs">
            Select a revision to diff it against the current content. Restoring
            creates a new revision; history is never rewritten.
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-1 min-h-0">
          {/* Revision list */}
          <div className="w-80 border-r overflow-y-auto shrink-0">
            {isLoading ? (
              <div className="p-4 text-sm text-muted-foreground">Loading…</div>
            ) : !revisions || revisions.length === 0 ? (
              <div className="p-4 text-sm text-muted-foreground">
                No revisions yet — history starts with the next save.
              </div>
            ) : (
              <ul>
                {revisions.map((r) => (
                  <li key={r.rev}>
                    <button
                      type="button"
                      className={`w-full text-left px-3 py-2 border-b text-xs hover:bg-accent/50 ${
                        r.rev === selectedRev ? "bg-accent" : ""
                      }`}
                      onClick={() => setSelectedRev(r.rev)}
                    >
                      <div className="flex items-center gap-2">
                        <span className="font-mono font-semibold">#{r.rev}</span>
                        <ViaBadge via={r.edited_via} />
                        {r.deleted && (
                          <Badge variant="destructive" className="text-[9px] h-4 px-1">
                            <Trash2 className="h-2.5 w-2.5 mr-0.5" />
                            deleted
                          </Badge>
                        )}
                        {r.restored_from !== null && (
                          <span className="text-muted-foreground">↩ from #{r.restored_from}</span>
                        )}
                      </div>
                      <div className="mt-0.5 text-muted-foreground">
                        {new Date(r.created_at).toLocaleString()}
                      </div>
                      <div className="mt-0.5 truncate">
                        {r.edited_by ?? <span className="text-muted-foreground">unattributed</span>}
                      </div>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>

          {/* Diff + actions */}
          <div className="flex-1 min-w-0 flex flex-col">
            {selected === null ? (
              <div className="flex-1 flex items-center justify-center text-sm text-muted-foreground">
                Select a revision to compare.
              </div>
            ) : selected.deleted ? (
              <div className="flex-1 flex items-center justify-center text-sm text-muted-foreground px-8 text-center">
                This is a delete tombstone — it records who deleted the
                definition. Pick a content revision to restore.
              </div>
            ) : selectedContent === null ? (
              <div className="flex-1 flex items-center justify-center text-sm text-muted-foreground">
                Loading revision…
              </div>
            ) : (
              <>
                <div className="flex items-center justify-between px-3 py-1.5 border-b text-xs text-muted-foreground shrink-0">
                  <span>
                    Revision #{selected.rev} (left) vs current (right)
                  </span>
                  <Button
                    size="sm"
                    className="h-7 text-xs"
                    disabled={restorePending || selectedContent === currentContent}
                    onClick={() => {
                      if (
                        confirm(
                          `Restore revision #${selected.rev}? The current content is preserved in history.`,
                        )
                      ) {
                        onRestore(selected.rev);
                      }
                    }}
                  >
                    <RotateCcw className="h-3.5 w-3.5 mr-1" />
                    {restorePending ? "Restoring…" : `Restore #${selected.rev}`}
                  </Button>
                </div>
                <div className="flex-1 min-h-0">
                  <WorkflowDiff
                    original={selectedContent}
                    modified={currentContent}
                    language={language}
                  />
                </div>
              </>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
