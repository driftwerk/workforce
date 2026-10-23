import { AlertTriangle } from "lucide-react";
import type { CoverageNote } from "@/lib/pipeline/costs";

/** What a panel's figures leave out: warnings boxed, the rest as muted lines. */
export function CoverageNotes({ notes }: { notes: CoverageNote[] }) {
  if (notes.length === 0) return null;
  return (
    <div className="space-y-2">
      {notes.map((note) =>
        note.warn ? (
          <p
            key={note.text}
            className="flex items-start gap-2 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm"
          >
            <AlertTriangle aria-hidden className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            {note.text}
          </p>
        ) : (
          <p key={note.text} className="text-xs text-muted-foreground">
            {note.text}
          </p>
        ),
      )}
    </div>
  );
}
