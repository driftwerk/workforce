import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  BookOpen,
  Download,
  FileText,
  Plus,
  Trash2,
  Upload,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { EmptyState } from "@/components/empty-state";
import {
  useAgentCorpora,
  useAgentCorpusDetail,
  useDeleteAgentCorpus,
  useDeleteAgentDocument,
  useUploadAgentCorpus,
} from "@/hooks/use-agent-training";
import type {
  CorpusSummary,
  DocumentRow,
  ProcessingStatus,
  UploadOutcome,
} from "@/hooks/use-agent-training";
import { trainingApi } from "@/lib/api/training";
import { ApiError } from "@/lib/api-client";

const SUPPORTED_EXTENSIONS =
  ".json,.pdf,.docx,.xlsx,.xls,.ods,.xlsb,.xlsm,.txt,.md,.markdown";
const SLUG_PATTERN = /^[A-Za-z0-9._-]+$/;
const FILENAME_TRUNCATE = 48;

interface AgentTrainingProps {
  agentId: string;
}

export function AgentTraining({ agentId }: AgentTrainingProps) {
  const [selectedCorpus, setSelectedCorpus] = useState<string | null>(null);
  const [uploadOpen, setUploadOpen] = useState(false);

  if (selectedCorpus !== null) {
    return (
      <CorpusDetailView
        agentId={agentId}
        corpusName={selectedCorpus}
        onBack={() => setSelectedCorpus(null)}
        onAddDocument={() => setUploadOpen(true)}
        uploadOpen={uploadOpen}
        setUploadOpen={setUploadOpen}
        prefilledCorpus={selectedCorpus}
      />
    );
  }

  return (
    <CorpusListView
      agentId={agentId}
      onSelectCorpus={setSelectedCorpus}
      uploadOpen={uploadOpen}
      setUploadOpen={setUploadOpen}
    />
  );
}

interface CorpusListViewProps {
  agentId: string;
  onSelectCorpus: (name: string) => void;
  uploadOpen: boolean;
  setUploadOpen: (open: boolean) => void;
}

function CorpusListView({
  agentId,
  onSelectCorpus,
  uploadOpen,
  setUploadOpen,
}: CorpusListViewProps) {
  const { data, isLoading, error } = useAgentCorpora(agentId);
  const corpora: CorpusSummary[] = useMemo(
    () => data?.corpora ?? [],
    [data?.corpora],
  );
  const deleteMutation = useDeleteAgentCorpus(agentId);
  const [deleteTarget, setDeleteTarget] = useState<CorpusSummary | null>(null);

  if (isLoading) {
    return <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>;
  }

  if (error) {
    return <TrainingError error={error} />;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h3 className="text-lg font-semibold">Training corpora</h3>
          <p className="text-sm text-muted-foreground">
            Reference material the agent retrieves automatically and cites in its answers.
          </p>
        </div>
        <Button onClick={() => setUploadOpen(true)}>
          <Plus className="mr-2 h-4 w-4" />
          Upload
        </Button>
      </div>

      {corpora.length === 0 ? (
        <EmptyState
          icon={BookOpen}
          title="No training corpora yet"
          description="Upload a PDF, DOCX, XLSX, JSON, TXT, or Markdown file to give this agent reference material."
          action={{ label: "Upload first corpus", onClick: () => setUploadOpen(true) }}
        />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Corpus
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-32">
                  Documents
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-32">
                  Chunks
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-48">
                  Created
                </th>
                <th className="w-32 px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {corpora.map((corpus) => (
                <tr
                  key={corpus.id}
                  className="border-b last:border-0 hover:bg-muted/30 cursor-pointer"
                  onClick={() => onSelectCorpus(corpus.name)}
                >
                  <td className="px-4 py-3 font-mono text-sm">{corpus.name}</td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">
                    {corpus.document_count}
                  </td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">
                    {corpus.chunk_count}
                  </td>
                  <td className="px-4 py-3 text-sm text-muted-foreground">
                    {formatTimestamp(corpus.created_at)}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={(e) => {
                        e.stopPropagation();
                        setDeleteTarget(corpus);
                      }}
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <UploadDialog
        agentId={agentId}
        open={uploadOpen}
        onOpenChange={setUploadOpen}
        prefilledCorpus={null}
      />

      <DeleteCorpusDialog
        target={deleteTarget}
        onCancel={() => setDeleteTarget(null)}
        onConfirm={() => {
          if (!deleteTarget) return;
          deleteMutation.mutate(deleteTarget.name, {
            onSuccess: () => setDeleteTarget(null),
          });
        }}
        isPending={deleteMutation.isPending}
      />
    </div>
  );
}

interface CorpusDetailViewProps {
  agentId: string;
  corpusName: string;
  onBack: () => void;
  onAddDocument: () => void;
  uploadOpen: boolean;
  setUploadOpen: (open: boolean) => void;
  prefilledCorpus: string;
}

function CorpusDetailView({
  agentId,
  corpusName,
  onBack,
  onAddDocument,
  uploadOpen,
  setUploadOpen,
  prefilledCorpus,
}: CorpusDetailViewProps) {
  const { data, isLoading, error } = useAgentCorpusDetail(agentId, corpusName);
  const deleteCorpusMutation = useDeleteAgentCorpus(agentId);
  const deleteDocumentMutation = useDeleteAgentDocument(agentId, corpusName);
  const [deleteCorpusOpen, setDeleteCorpusOpen] = useState(false);
  const [deleteDocTarget, setDeleteDocTarget] = useState<DocumentRow | null>(null);

  const documents: DocumentRow[] = useMemo(
    () => data?.documents ?? [],
    [data?.documents],
  );

  if (isLoading) {
    return <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>;
  }
  if (error) {
    return <TrainingError error={error} />;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="sm" onClick={onBack}>
            <ArrowLeft className="mr-1 h-4 w-4" />
            Corpora
          </Button>
          <div>
            <h3 className="text-lg font-semibold font-mono">{corpusName}</h3>
            <p className="text-sm text-muted-foreground">
              {documents.length} document{documents.length === 1 ? "" : "s"}
              {data?.created_at ? ` · created ${formatTimestamp(data.created_at)}` : ""}
            </p>
          </div>
        </div>
        <div className="flex gap-2">
          <Button onClick={onAddDocument}>
            <Plus className="mr-2 h-4 w-4" />
            Add document
          </Button>
          <Button variant="outline" onClick={() => setDeleteCorpusOpen(true)}>
            <Trash2 className="mr-2 h-4 w-4" />
            Delete corpus
          </Button>
        </div>
      </div>

      {documents.length === 0 ? (
        <EmptyState
          icon={FileText}
          title="No documents in this corpus"
          description="Add a file to start populating this corpus."
          action={{ label: "Add document", onClick: onAddDocument }}
        />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-24">
                  ID
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Filename
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-24">
                  Size
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-20">
                  Chunks
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-28">
                  Status
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-40">
                  Uploaded
                </th>
                <th className="w-32 px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {documents.map((doc) => (
                <tr key={doc.id} className="border-b last:border-0">
                  <td className="px-4 py-3 font-mono text-xs">{doc.short_id}</td>
                  <td className="px-4 py-3 text-sm" title={doc.filename}>
                    {truncate(doc.filename, FILENAME_TRUNCATE)}
                  </td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">
                    {humanSize(doc.size_bytes)}
                  </td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">
                    {doc.chunk_count}
                  </td>
                  <td className="px-4 py-3">
                    <StatusBadge status={doc.processing_status} error={doc.processing_error} />
                  </td>
                  <td className="px-4 py-3 text-sm text-muted-foreground">
                    {formatTimestamp(doc.uploaded_at)}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <div className="flex justify-end gap-1">
                      <a
                        href={trainingApi.documentDownloadUrl(agentId, doc.id)}
                        download={doc.filename}
                      >
                        <Button variant="ghost" size="sm" title="Download original">
                          <Download className="h-4 w-4" />
                        </Button>
                      </a>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Delete document"
                        onClick={() => setDeleteDocTarget(doc)}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <UploadDialog
        agentId={agentId}
        open={uploadOpen}
        onOpenChange={setUploadOpen}
        prefilledCorpus={prefilledCorpus}
      />

      <Dialog open={deleteCorpusOpen} onOpenChange={setDeleteCorpusOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete corpus</DialogTitle>
            <DialogDescription>
              Removes every document in <span className="font-mono">{corpusName}</span> plus
              their chunks and originals. The agent immediately stops citing this material.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteCorpusOpen(false)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteCorpusMutation.isPending}
              onClick={() =>
                deleteCorpusMutation.mutate(corpusName, {
                  onSuccess: () => {
                    setDeleteCorpusOpen(false);
                    onBack();
                  },
                })
              }
            >
              {deleteCorpusMutation.isPending ? "Deleting…" : "Delete corpus"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <DeleteDocumentDialog
        target={deleteDocTarget}
        onCancel={() => setDeleteDocTarget(null)}
        onConfirm={() => {
          if (!deleteDocTarget) return;
          deleteDocumentMutation.mutate(deleteDocTarget.id, {
            onSuccess: () => setDeleteDocTarget(null),
          });
        }}
        isPending={deleteDocumentMutation.isPending}
      />
    </div>
  );
}

interface UploadDialogProps {
  agentId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  prefilledCorpus: string | null;
}

function UploadDialog({
  agentId,
  open,
  onOpenChange,
  prefilledCorpus,
}: UploadDialogProps) {
  const [corpus, setCorpus] = useState<string>(prefilledCorpus ?? "");
  const [file, setFile] = useState<File | null>(null);
  const [outcome, setOutcome] = useState<UploadOutcome | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const mutation = useUploadAgentCorpus(agentId);

  // Sync the corpus field with the parent-supplied prefill whenever the
  // dialog opens. Effect avoids the StrictMode warning a render-time
  // setCorpus would trigger.
  useEffect(() => {
    if (open && prefilledCorpus !== null) setCorpus(prefilledCorpus);
  }, [open, prefilledCorpus]);

  const slugError = corpus.length === 0
    ? null
    : SLUG_PATTERN.test(corpus)
      ? null
      : "Slug may contain ASCII letters, digits, `-`, `_`, `.` only.";
  const canSubmit = !!file && corpus.length > 0 && slugError === null && !mutation.isPending;

  function reset() {
    setCorpus(prefilledCorpus ?? "");
    setFile(null);
    setOutcome(null);
    mutation.reset();
    if (inputRef.current) inputRef.current.value = "";
  }

  function handleClose(next: boolean) {
    if (!next) reset();
    onOpenChange(next);
  }

  return (
    <Dialog open={open} onOpenChange={handleClose}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Upload to training corpus</DialogTitle>
          <DialogDescription>
            Supported formats: PDF, DOCX, XLSX (and siblings), TXT, Markdown, JSON. The file
            is parsed into chunks the agent retrieves and cites automatically.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-2">
          <div className="space-y-2">
            <label className="text-sm font-medium" htmlFor="corpus-name">
              Corpus name
            </label>
            <input
              id="corpus-name"
              type="text"
              value={corpus}
              disabled={prefilledCorpus !== null}
              onChange={(e) => setCorpus(e.target.value.trim())}
              placeholder="e.g. handbook, product-catalogue"
              className="w-full rounded-md border border-input bg-background px-3 py-2 text-sm font-mono focus:outline-none focus:ring-2 focus:ring-ring disabled:cursor-not-allowed disabled:opacity-60"
            />
            {slugError && (
              <p className="text-sm text-destructive">{slugError}</p>
            )}
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium" htmlFor="corpus-file">
              File
            </label>
            <input
              id="corpus-file"
              ref={inputRef}
              type="file"
              accept={SUPPORTED_EXTENSIONS}
              onChange={(e) => setFile(e.target.files?.[0] ?? null)}
              className="block w-full text-sm text-muted-foreground file:mr-3 file:rounded-md file:border-0 file:bg-secondary file:px-3 file:py-2 file:text-sm file:font-medium file:text-secondary-foreground hover:file:bg-secondary/80"
            />
            {file && (
              <p className="text-xs text-muted-foreground">
                {file.name} ({humanSize(file.size)})
              </p>
            )}
          </div>

          {mutation.isError && (
            <p className="text-sm text-destructive">
              Upload failed: {extractApiErrorMessage(mutation.error)}
            </p>
          )}

          {outcome && (
            <div className="rounded-md border border-success/30 bg-success/10 p-3 text-sm">
              Added <span className="font-mono">{outcome.filename}</span> to corpus{" "}
              <span className="font-mono">{outcome.corpus}</span> — {outcome.written} new
              chunk{outcome.written === 1 ? "" : "s"}, {outcome.unchanged} unchanged,{" "}
              {outcome.deleted} removed.
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => handleClose(false)}>
            {outcome ? "Close" : "Cancel"}
          </Button>
          {!outcome && (
            <Button
              disabled={!canSubmit}
              onClick={() => {
                if (!file) return;
                mutation.mutate(
                  { corpus, file },
                  {
                    onSuccess: (data) => setOutcome(data),
                  },
                );
              }}
            >
              {mutation.isPending ? "Uploading…" : (
                <>
                  <Upload className="mr-2 h-4 w-4" />
                  Upload
                </>
              )}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface DeleteCorpusDialogProps {
  target: CorpusSummary | null;
  onCancel: () => void;
  onConfirm: () => void;
  isPending: boolean;
}

function DeleteCorpusDialog({ target, onCancel, onConfirm, isPending }: DeleteCorpusDialogProps) {
  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && onCancel()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete corpus</DialogTitle>
          <DialogDescription>
            {target ? (
              <>
                Permanently removes <span className="font-mono">{target.name}</span> and all{" "}
                {target.document_count} document{target.document_count === 1 ? "" : "s"} ({target.chunk_count}
                {" "}chunks). The agent immediately stops citing this material.
              </>
            ) : null}
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={onCancel}>
            Cancel
          </Button>
          <Button variant="destructive" disabled={isPending} onClick={onConfirm}>
            {isPending ? "Deleting…" : "Delete"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface DeleteDocumentDialogProps {
  target: DocumentRow | null;
  onCancel: () => void;
  onConfirm: () => void;
  isPending: boolean;
}

function DeleteDocumentDialog({
  target,
  onCancel,
  onConfirm,
  isPending,
}: DeleteDocumentDialogProps) {
  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && onCancel()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete document</DialogTitle>
          <DialogDescription>
            {target ? (
              <>
                Removes <span className="font-mono">{target.filename}</span> and its{" "}
                {target.chunk_count} chunk{target.chunk_count === 1 ? "" : "s"} from the
                corpus.
              </>
            ) : null}
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={onCancel}>
            Cancel
          </Button>
          <Button variant="destructive" disabled={isPending} onClick={onConfirm}>
            {isPending ? "Deleting…" : "Delete"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function StatusBadge({
  status,
  error,
}: {
  status: ProcessingStatus;
  error?: string | null;
}) {
  const variant = STATUS_VARIANTS[status];
  const label = STATUS_LABELS[status];
  return (
    <Badge variant={variant} title={error ?? undefined}>
      {label}
    </Badge>
  );
}

const STATUS_VARIANTS: Record<ProcessingStatus, "default" | "secondary" | "destructive" | "outline"> = {
  ready: "default",
  processing: "secondary",
  pending: "outline",
  failed: "destructive",
};

const STATUS_LABELS: Record<ProcessingStatus, string> = {
  ready: "Ready",
  processing: "Processing",
  pending: "Pending",
  failed: "Failed",
};

function TrainingError({ error }: { error: unknown }) {
  const message = extractApiErrorMessage(error);
  const status = error instanceof ApiError ? error.status : 0;
  if (status === 503) {
    return (
      <div className="rounded-md border border-warning/30 bg-warning/10 p-4 text-sm">
        Training is disabled for this hub. Enable <span className="font-mono">[training] enabled</span>{" "}
        in <span className="font-mono">workforce.toml</span> and restart the hub.
      </div>
    );
  }
  return (
    <div className="rounded-md border border-destructive/30 bg-destructive/10 p-4 text-sm text-destructive">
      Failed to load training corpora: {message}
    </div>
  );
}

function extractApiErrorMessage(error: unknown): string {
  if (!error) return "Unknown error";
  if (typeof error === "string") return error;
  if (error instanceof Error) {
    try {
      const parsed = JSON.parse(error.message) as { error?: string };
      if (parsed.error) return parsed.error;
    } catch {
      // not JSON — fall through to raw message
    }
    return error.message;
  }
  return "Unknown error";
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 1)}…`;
}

function humanSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const KIB = 1024;
  if (bytes < KIB) return `${bytes} B`;
  if (bytes < KIB ** 2) return `${(bytes / KIB).toFixed(1)} KiB`;
  if (bytes < KIB ** 3) return `${(bytes / KIB ** 2).toFixed(1)} MiB`;
  if (bytes < KIB ** 4) return `${(bytes / KIB ** 3).toFixed(1)} GiB`;
  return `${(bytes / KIB ** 4).toFixed(1)} TiB`;
}

function formatTimestamp(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}
