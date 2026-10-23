import { useMemo, useState } from "react";
import { Trash2 } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useLessons, useDeleteLesson } from "@/hooks/use-lessons";
import {
  LEARNING_KINDS,
  type Lesson,
  type LessonsFilter,
} from "@/lib/api/lessons";

const PAGE_SIZE = 50;

const SELECT_CLASS =
  "h-9 rounded-md border border-input bg-background px-3 py-1.5 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring";

function kindLabel(kind: string): string {
  return kind.replace(/_/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}

function ContentCell({ content }: { content: string }) {
  const [expanded, setExpanded] = useState(false);
  const long = content.length > 160;
  if (!long) return <span className="text-sm">{content}</span>;
  return (
    <div className="text-sm">
      <span>{expanded ? content : `${content.slice(0, 160)}…`}</span>{" "}
      <button
        onClick={() => setExpanded((v) => !v)}
        className="text-xs text-primary hover:underline"
      >
        {expanded ? "less" : "more"}
      </button>
    </div>
  );
}

interface Props {
  /** Current agent from the page selector ("all" or an agent id). */
  agent: string;
}

export function LessonBrowser({ agent }: Props) {
  const [kind, setKind] = useState("");
  const [applied, setApplied] = useState<"" | "0" | "1">("");
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const [toDelete, setToDelete] = useState<Lesson | null>(null);

  const filter: LessonsFilter = useMemo(
    () => ({
      agent,
      kind: kind || undefined,
      applied: applied || undefined,
      q: q || undefined,
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
    }),
    [agent, kind, applied, q, page],
  );

  const { data, isLoading, error } = useLessons(filter);
  const del = useDeleteLesson();

  const lessons = data?.lessons ?? [];
  const total = data?.total ?? 0;
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  function resetPage<T>(setter: (v: T) => void) {
    return (v: T) => {
      setter(v);
      setPage(0);
    };
  }

  async function confirmDelete() {
    if (!toDelete) return;
    await del.mutateAsync(toDelete.id);
    setToDelete(null);
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Lesson browser</CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap items-center gap-3">
          <select
            value={kind}
            onChange={(e) => resetPage(setKind)(e.target.value)}
            className={SELECT_CLASS}
          >
            <option value="">All kinds</option>
            {LEARNING_KINDS.map((k) => (
              <option key={k} value={k}>
                {kindLabel(k)}
              </option>
            ))}
          </select>

          <select
            value={applied}
            onChange={(e) =>
              resetPage(setApplied)(e.target.value as "" | "0" | "1")
            }
            className={SELECT_CLASS}
          >
            <option value="">Any applied</option>
            <option value="0">Never applied</option>
            <option value="1">Applied ≥1</option>
          </select>

          <Input
            value={q}
            onChange={(e) => resetPage(setQ)(e.target.value)}
            placeholder="Search content / triggers…"
            className="h-9 w-64"
          />

          <span className="ml-auto text-sm text-muted-foreground">
            {total.toLocaleString()} lesson{total !== 1 ? "s" : ""}
          </span>
        </div>

        {error ? (
          <div className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
            Failed to load lessons:{" "}
            {error instanceof Error ? error.message : "Unknown error"}
          </div>
        ) : (
          <div className="overflow-x-auto rounded-lg border">
            <table className="w-full text-sm">
              <thead className="bg-muted/40">
                <tr>
                  {["Content", "Triggers", "Kind", "Conf.", "Applied", "Date", ""].map(
                    (h) => (
                      <th
                        key={h}
                        className="px-3 py-2 text-left text-xs font-medium text-muted-foreground whitespace-nowrap"
                      >
                        {h}
                      </th>
                    ),
                  )}
                </tr>
              </thead>
              <tbody className="divide-y divide-border">
                {isLoading && lessons.length === 0 ? (
                  <tr>
                    <td colSpan={7} className="px-3 py-6 text-center text-muted-foreground">
                      Loading…
                    </td>
                  </tr>
                ) : lessons.length === 0 ? (
                  <tr>
                    <td colSpan={7} className="px-3 py-6 text-center text-muted-foreground">
                      No lessons match the current filters.
                    </td>
                  </tr>
                ) : (
                  lessons.map((l) => (
                    <tr key={l.id} className="hover:bg-muted/20 transition-colors align-top">
                      <td className="px-3 py-2 max-w-[420px]">
                        <ContentCell content={l.content} />
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex flex-wrap gap-1">
                          {l.trigger_tags.length === 0 ? (
                            <span className="text-xs text-muted-foreground italic">—</span>
                          ) : (
                            l.trigger_tags.map((t) => (
                              <Badge key={t} variant="outline" className="font-mono text-[10px]">
                                {t}
                              </Badge>
                            ))
                          )}
                        </div>
                      </td>
                      <td className="px-3 py-2 whitespace-nowrap text-xs">
                        {l.learning_kind ? (
                          kindLabel(l.learning_kind)
                        ) : (
                          <span className="text-muted-foreground italic">—</span>
                        )}
                      </td>
                      <td className="px-3 py-2 whitespace-nowrap text-xs font-mono tabular-nums">
                        {l.confidence.toFixed(2)}
                      </td>
                      <td className="px-3 py-2 whitespace-nowrap text-xs tabular-nums">
                        {l.applied > 0 ? (
                          <Badge variant="success">×{l.applied}</Badge>
                        ) : (
                          <span className="text-muted-foreground">0</span>
                        )}
                      </td>
                      <td className="px-3 py-2 whitespace-nowrap text-xs font-mono text-muted-foreground">
                        {l.date}
                      </td>
                      <td className="px-3 py-2 whitespace-nowrap text-right">
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7 text-muted-foreground hover:text-destructive"
                          onClick={() => setToDelete(l)}
                          title="Delete lesson"
                        >
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        )}

        {totalPages > 1 && (
          <div className="flex items-center justify-between text-sm text-muted-foreground">
            <span>
              Page {page + 1} of {totalPages}
            </span>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={page === 0}
                onClick={() => setPage((p) => p - 1)}
              >
                Previous
              </Button>
              <Button
                variant="outline"
                size="sm"
                disabled={page >= totalPages - 1}
                onClick={() => setPage((p) => p + 1)}
              >
                Next
              </Button>
            </div>
          </div>
        )}
      </CardContent>

      <Dialog open={!!toDelete} onOpenChange={(open) => !open && setToDelete(null)}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>Delete lesson?</DialogTitle>
            <DialogDescription>
              This permanently forgets the lesson from the corpus. This cannot be
              undone.
            </DialogDescription>
          </DialogHeader>
          {toDelete && (
            <p className="rounded-md border border-border bg-muted/30 p-3 text-sm">
              {toDelete.content.length > 240
                ? `${toDelete.content.slice(0, 240)}…`
                : toDelete.content}
            </p>
          )}
          {del.isError && (
            <p className="text-sm text-destructive">
              Delete failed:{" "}
              {del.error instanceof Error ? del.error.message : "Unknown error"}
            </p>
          )}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setToDelete(null)}
              disabled={del.isPending}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={confirmDelete}
              disabled={del.isPending}
            >
              {del.isPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  );
}
