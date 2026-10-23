import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { Plus, Search, Trash2, Pencil, X } from "lucide-react";

import {
  artifactsApi,
  ARTIFACT_TYPES,
  VISIBILITIES,
  type Artifact,
  type ArtifactFilters,
  type ArtifactSummary,
  type ArtifactType,
  type Visibility,
} from "@/lib/api/artifacts";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

function visibilityVariant(v: Visibility): "default" | "secondary" | "outline" {
  return v === "public" ? "default" : v === "team" ? "secondary" : "outline";
}

interface EditorState {
  mode: "create" | "edit";
  id?: string;
  title: string;
  artifact_type: ArtifactType;
  visibility: Visibility;
  tags: string;
  content: string;
}

const emptyEditor: EditorState = {
  mode: "create",
  title: "",
  artifact_type: "notes",
  visibility: "private",
  tags: "",
  content: "",
};

export default function ArtifactsPage() {
  const qc = useQueryClient();
  const [filters, setFilters] = useState<ArtifactFilters>({});
  const [searchInput, setSearchInput] = useState("");
  const [activeSearch, setActiveSearch] = useState("");
  const [openId, setOpenId] = useState<string | null>(null);
  const [editor, setEditor] = useState<EditorState | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  const listKey = ["artifacts", filters, activeSearch];
  const { data, isLoading, error } = useQuery({
    queryKey: listKey,
    queryFn: async () => {
      if (activeSearch.trim()) {
        const r = await artifactsApi.search(activeSearch, filters);
        return r.results;
      }
      const r = await artifactsApi.list(filters);
      return r.artifacts;
    },
    staleTime: 15_000,
  });

  const detail = useQuery({
    queryKey: ["artifact", openId],
    queryFn: () => artifactsApi.get(openId!),
    enabled: !!openId,
  });

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ["artifacts"] });
    if (openId) qc.invalidateQueries({ queryKey: ["artifact", openId] });
  };

  const createMut = useMutation({
    mutationFn: () => {
      const e = editor!;
      return artifactsApi.create({
        title: e.title || "Untitled",
        content: e.content,
        artifact_type: e.artifact_type,
        visibility: e.visibility,
        tags: e.tags
          .split(",")
          .map((t) => t.trim())
          .filter(Boolean),
      });
    },
    onSuccess: () => {
      setEditor(null);
      invalidate();
    },
  });

  const updateMut = useMutation({
    mutationFn: () => {
      const e = editor!;
      return artifactsApi.update(e.id!, {
        content: e.content,
        visibility: e.visibility,
      });
    },
    onSuccess: () => {
      setEditor(null);
      invalidate();
    },
  });

  const deleteMut = useMutation({
    mutationFn: (id: string) => artifactsApi.del(id),
    onSuccess: () => {
      setConfirmDelete(null);
      setOpenId(null);
      invalidate();
    },
  });

  const items = data ?? [];
  const saving = createMut.isPending || updateMut.isPending;

  const openCreate = () => setEditor({ ...emptyEditor });
  const openEdit = (a: Artifact) =>
    setEditor({
      mode: "edit",
      id: a.id,
      title: a.title,
      artifact_type: a.artifact_type,
      visibility: a.visibility,
      tags: a.tags.join(", "),
      content: a.content,
    });

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Artifacts</h1>
          <p className="text-sm text-muted-foreground">
            Shared knowledge base — specs, notes, snippets &amp; more.
          </p>
        </div>
        <Button onClick={openCreate}>
          <Plus className="h-4 w-4 mr-1" /> New artifact
        </Button>
      </div>

      {/* Search + filters */}
      <div className="flex flex-wrap items-center gap-2">
        <form
          className="flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            setActiveSearch(searchInput);
          }}
        >
          <div className="relative">
            <Search className="absolute left-2 top-2.5 h-4 w-4 text-muted-foreground" />
            <Input
              className="pl-8 w-72"
              placeholder="Semantic search…"
              value={searchInput}
              onChange={(e) => setSearchInput(e.target.value)}
            />
          </div>
          <Button type="submit" variant="secondary">
            Search
          </Button>
          {activeSearch && (
            <Button
              type="button"
              variant="ghost"
              onClick={() => {
                setActiveSearch("");
                setSearchInput("");
              }}
            >
              <X className="h-4 w-4" />
            </Button>
          )}
        </form>

        <select
          className="h-9 rounded-md border bg-background px-2 text-sm"
          value={filters.type ?? ""}
          onChange={(e) =>
            setFilters((f) => ({ ...f, type: (e.target.value || undefined) as ArtifactType | undefined }))
          }
        >
          <option value="">All types</option>
          {ARTIFACT_TYPES.map((t) => (
            <option key={t} value={t}>
              {t}
            </option>
          ))}
        </select>

        <select
          className="h-9 rounded-md border bg-background px-2 text-sm"
          value={filters.visibility ?? ""}
          onChange={(e) =>
            setFilters((f) => ({
              ...f,
              visibility: (e.target.value || undefined) as Visibility | undefined,
            }))
          }
        >
          <option value="">All visibility</option>
          {VISIBILITIES.map((v) => (
            <option key={v} value={v}>
              {v}
            </option>
          ))}
        </select>

        <Input
          className="w-40"
          placeholder="tag…"
          value={filters.tag ?? ""}
          onChange={(e) => setFilters((f) => ({ ...f, tag: e.target.value || undefined }))}
        />
      </div>

      {/* Grid */}
      {isLoading ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : error ? (
        <p className="text-sm text-destructive">
          Failed to load artifacts. The knowledge base may be unavailable (Postgres not configured).
        </p>
      ) : items.length === 0 ? (
        <p className="text-sm text-muted-foreground">No artifacts found.</p>
      ) : (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {items.map((a) => (
            <ArtifactCard key={a.id} a={a} onOpen={() => setOpenId(a.id)} />
          ))}
        </div>
      )}

      {/* Detail dialog */}
      <Dialog open={!!openId} onOpenChange={(o) => !o && setOpenId(null)}>
        <DialogContent className="max-w-3xl max-h-[85vh] overflow-y-auto">
          {detail.data && (
            <>
              <DialogHeader>
                <DialogTitle className="flex items-center gap-2">
                  {detail.data.title}
                  <Badge variant="outline">{detail.data.artifact_type}</Badge>
                  <Badge variant={visibilityVariant(detail.data.visibility)}>
                    {detail.data.visibility}
                  </Badge>
                </DialogTitle>
              </DialogHeader>
              <div className="flex flex-wrap gap-1 mb-2">
                {detail.data.tags.map((t) => (
                  <Badge key={t} variant="secondary">
                    {t}
                  </Badge>
                ))}
              </div>
              <div className="prose prose-sm dark:prose-invert max-w-none">
                <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
                  {detail.data.content || "_(empty)_"}
                </ReactMarkdown>
              </div>
              <DialogFooter className="gap-2">
                <span className="text-xs text-muted-foreground mr-auto">
                  v{detail.data.version} · owner {detail.data.owner_id} · by{" "}
                  {detail.data.last_modified_by}
                </span>
                <Button variant="outline" onClick={() => openEdit(detail.data!)}>
                  <Pencil className="h-4 w-4 mr-1" /> Edit
                </Button>
                <Button
                  variant="destructive"
                  onClick={() => setConfirmDelete(detail.data!.id)}
                >
                  <Trash2 className="h-4 w-4 mr-1" /> Delete
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Editor dialog */}
      <Dialog open={!!editor} onOpenChange={(o) => !o && setEditor(null)}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              {editor?.mode === "create" ? "New artifact" : "Edit artifact"}
            </DialogTitle>
          </DialogHeader>
          {editor && (
            <div className="space-y-3">
              <Input
                placeholder="Title"
                value={editor.title}
                disabled={editor.mode === "edit"}
                onChange={(e) => setEditor({ ...editor, title: e.target.value })}
              />
              <div className="flex gap-2">
                <select
                  className="h-9 rounded-md border bg-background px-2 text-sm flex-1"
                  value={editor.artifact_type}
                  disabled={editor.mode === "edit"}
                  onChange={(e) =>
                    setEditor({ ...editor, artifact_type: e.target.value as ArtifactType })
                  }
                >
                  {ARTIFACT_TYPES.map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                </select>
                <select
                  className="h-9 rounded-md border bg-background px-2 text-sm flex-1"
                  value={editor.visibility}
                  onChange={(e) =>
                    setEditor({ ...editor, visibility: e.target.value as Visibility })
                  }
                >
                  {VISIBILITIES.map((v) => (
                    <option key={v} value={v}>
                      {v}
                    </option>
                  ))}
                </select>
              </div>
              {editor.mode === "create" && (
                <Input
                  placeholder="tags, comma, separated"
                  value={editor.tags}
                  onChange={(e) => setEditor({ ...editor, tags: e.target.value })}
                />
              )}
              <textarea
                className="w-full h-64 rounded-md border bg-background p-2 font-mono text-sm"
                placeholder="# Markdown content"
                value={editor.content}
                onChange={(e) => setEditor({ ...editor, content: e.target.value })}
              />
            </div>
          )}
          <DialogFooter>
            <Button variant="ghost" onClick={() => setEditor(null)}>
              Cancel
            </Button>
            <Button
              disabled={saving}
              onClick={() => (editor?.mode === "create" ? createMut.mutate() : updateMut.mutate())}
            >
              {saving ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Delete confirm */}
      <Dialog open={!!confirmDelete} onOpenChange={(o) => !o && setConfirmDelete(null)}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>Delete artifact?</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">
            This permanently deletes <code>{confirmDelete}</code>. This cannot be undone.
          </p>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setConfirmDelete(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteMut.isPending}
              onClick={() => confirmDelete && deleteMut.mutate(confirmDelete)}
            >
              {deleteMut.isPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function ArtifactCard({ a, onOpen }: { a: ArtifactSummary; onOpen: () => void }) {
  const updated = useMemo(() => new Date(a.updated_at).toLocaleDateString(), [a.updated_at]);
  return (
    <Card className="cursor-pointer hover:border-primary/50 transition-colors" onClick={onOpen}>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center justify-between gap-2">
          <span className="truncate">{a.title}</span>
          <Badge variant={visibilityVariant(a.visibility)}>{a.visibility}</Badge>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex flex-wrap items-center gap-1">
          <Badge variant="outline">{a.artifact_type}</Badge>
          {a.tags.slice(0, 4).map((t) => (
            <Badge key={t} variant="secondary">
              {t}
            </Badge>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">
          updated {updated}
          {a.score != null && ` · score ${a.score.toFixed(2)}`}
        </p>
      </CardContent>
    </Card>
  );
}
