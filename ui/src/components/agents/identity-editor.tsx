import { useState, useCallback, useEffect } from "react";
import Editor from "@monaco-editor/react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Save, FileText, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  useAgentIdentity,
  useSaveAgentIdentity,
  useIdentityRevisions,
  useRestoreIdentityRevision,
} from "@/hooks/use-agents";
import { agentsApi } from "@/lib/api/agents";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { EDITOR_OPTIONS } from "@/lib/monaco-setup";

const IDENTITY_FILES = [
  "SOUL.md",
  "AGENTS.md",
  "TOOLS.md",
  "IDENTITY.md",
  "WELCOME.md",
];

interface IdentityEditorProps {
  agentId: string;
}

export function IdentityEditor({ agentId }: IdentityEditorProps) {
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [editedContent, setEditedContent] = useState("");
  const [dirty, setDirty] = useState(false);
  const [showHistory, setShowHistory] = useState(false);

  const { data: content, isLoading } = useAgentIdentity(agentId, selectedFile);
  const saveMutation = useSaveAgentIdentity(agentId);
  // Only fetch revisions while the dialog is open.
  const revisionsQuery = useIdentityRevisions(agentId, showHistory ? selectedFile : null);
  const restoreMutation = useRestoreIdentityRevision(agentId);

  useEffect(() => {
    if (content !== undefined && typeof content === "string") {
      setEditedContent(content);
      setDirty(false);
    }
  }, [content]);

  const handleEditorChange = useCallback((val: string | undefined) => {
    setEditedContent(val ?? "");
    setDirty(true);
  }, []);

  function handleSave() {
    if (!selectedFile || !dirty) return;
    saveMutation.mutate(
      { file: selectedFile, content: editedContent },
      { onSuccess: () => setDirty(false) },
    );
  }

  return (
    <div className="space-y-4">
      {/* File selector */}
      <div className="flex items-center gap-2 flex-wrap">
        {IDENTITY_FILES.map((file) => (
          <Button
            key={file}
            variant={selectedFile === file ? "default" : "outline"}
            size="sm"
            onClick={() => {
              setSelectedFile(file);
              setDirty(false);
            }}
          >
            <FileText className="h-3.5 w-3.5 mr-1" />
            {file}
          </Button>
        ))}
      </div>

      {!selectedFile ? (
        <div className="text-sm text-muted-foreground py-8 text-center">
          Select an identity file to view and edit.
        </div>
      ) : (
        <>
          {/* Save toolbar */}
          <div className="flex items-center justify-between">
            <span className="text-sm font-medium">{selectedFile}</span>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => setShowHistory(true)}
              >
                <History className="h-3.5 w-3.5 mr-1" />
                History
              </Button>
              <Button
                size="sm"
                disabled={!dirty || saveMutation.isPending}
                onClick={handleSave}
              >
                <Save className="h-3.5 w-3.5 mr-1" />
                {saveMutation.isPending ? "Saving..." : "Save"}
              </Button>
            </div>
          </div>

          {/* Split view */}
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-4" style={{ minHeight: 500 }}>
            {/* Editor */}
            <div className="border rounded-lg overflow-hidden">
              {isLoading ? (
                <div className="flex items-center justify-center h-full text-muted-foreground text-sm">
                  Loading...
                </div>
              ) : (
                <Editor
                  language="markdown"
                  theme="vs-dark"
                  value={editedContent}
                  onChange={handleEditorChange}
                  options={EDITOR_OPTIONS}
                />
              )}
            </div>

            {/* Preview */}
            <div className="border rounded-lg p-4 overflow-auto bg-card" style={{ maxHeight: 500 }}>
              <div className="prose prose-sm dark:prose-invert max-w-none">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>
                  {editedContent}
                </ReactMarkdown>
              </div>
            </div>
          </div>

          <RevisionHistoryDialog
            open={showHistory}
            onOpenChange={setShowHistory}
            title={`${agentId}/${selectedFile}`}
            revisions={revisionsQuery.data?.revisions}
            isLoading={revisionsQuery.isLoading}
            currentContent={editedContent}
            language="markdown"
            getContent={(rev) =>
              agentsApi
                .getIdentityRevision(agentId, selectedFile, rev)
                .then((r) => r.content ?? "")
            }
            onRestore={(rev) =>
              restoreMutation.mutate(
                { file: selectedFile, rev },
                {
                  onSuccess: () => setShowHistory(false),
                  onError: (err) =>
                    alert(
                      `Restore failed: ${err instanceof Error ? err.message : String(err)}`,
                    ),
                },
              )
            }
            restorePending={restoreMutation.isPending}
          />
        </>
      )}
    </div>
  );
}
