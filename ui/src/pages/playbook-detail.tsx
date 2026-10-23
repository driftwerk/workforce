import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router";
import { ReactFlowProvider } from "@xyflow/react";
import { ArrowLeft, Save, Code, Eye, Columns2, Diff, Trash2, History } from "lucide-react";
import { VisibilitySelect } from "@/components/sharing/visibility-select";
import {
  definitionVisibility,
  definitionWord,
  visibilityChoices,
} from "@/lib/access";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  usePlaybooks,
  usePlaybookSource,
  useSavePlaybook,
  usePlaybookRevisions,
  useRestorePlaybookRevision,
} from "@/hooks/use-playbooks";
import { playbooksApi } from "@/lib/api/playbooks";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { usePlaybookEditorStore, type ViewMode } from "@/stores/playbook-editor-store";
import { PlaybookBuilder } from "@/components/playbooks/playbook-builder";
import { ButtonEditorPanel } from "@/components/playbooks/button-editor-panel";
import { TomlEditor } from "@/components/workflows/toml-editor";
import { WorkflowDiff } from "@/components/workflows/workflow-diff";
import { DeletePlaybookDialog } from "@/components/playbooks/delete-playbook-dialog";
import { playbookDefToToml } from "@/lib/playbooks/graph-to-toml";
import type { PlaybookDef } from "@/lib/api/playbooks";

// Starter TOML for an unsaved draft; the placeholder id is meant to be replaced.
// The commented block shows how custom buttons branch the flow.
const NEW_PLAYBOOK_STUB = `[playbook]
id = "my-playbook"
name = "New Playbook"
description = ""

[[step]]
id = "start"
title = "First step"
# artifact = "output.md"   # optional: a file this step writes for later steps
# model = "powerful"        # optional: a tier or provider-routed tier for this step
prompt = """
Describe what you should do in this step and what to discuss with the user.
"""

# A branching step — custom buttons jump to other steps or end the playbook.
# Set keep_continue = false to drop the default Continue (a forced choice).
#
# [[step]]
# id = "choose"
# title = "Pick a path"
# prompt = "Ask the user which way to go, then wait for their button."
# keep_continue = false
#
#   [[step.button]]
#   label = "✅ Approve"
#   style = "primary"
#   goto = "start"
#
#   [[step.button]]
#   label = "Done"
#   finish = true
`;

export default function PlaybookDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const isNew = id === "new";

  const [searchParams] = useSearchParams();
  const { data: playbooksData } = usePlaybooks();
  // Owner disambiguates which member's definition this is (two members may share
  // an id); fall back to the list entry, else a system/global def.
  const owner =
    searchParams.get("owner") ??
    playbooksData?.playbooks.find((p) => p.id === id)?.owner ??
    null;
  const { data: source, isLoading } = usePlaybookSource(isNew ? undefined : id, owner);
  const saveMutation = useSavePlaybook();

  const store = usePlaybookEditorStore();
  const [savedSource, setSavedSource] = useState("");
  const [showDiff, setShowDiff] = useState(false);
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [activeStepId, setActiveStepId] = useState<string | null>(null);
  const [showHistory, setShowHistory] = useState(false);
  // Only fetch revisions while the dialog is open.
  const revisionsQuery = usePlaybookRevisions(showHistory && !isNew ? id : undefined, owner);
  const restoreMutation = useRestorePlaybookRevision();

  // Visibility (private/global) is storage metadata, not part of the TOML. Track
  // an override the toggle sets; default to the loaded def's visibility, else
  // private (a new draft is private until published) — the owner-null branch is
  // only the transient fallback for a system def not yet in the loaded list.
  const [visibilityOverride, setVisibilityOverride] = useState<string | null>(null);
  const loadedVisibility =
    playbooksData?.playbooks.find((p) => p.id === id && (p.owner ?? null) === owner)
      ?.visibility ?? (isNew || owner ? "private" : "global");
  const visibility = visibilityOverride ?? loadedVisibility;
  const visibilityDirty = visibilityOverride !== null && visibilityOverride !== loadedVisibility;
  useEffect(() => setVisibilityOverride(null), [id, owner]);

  const existingIds = useMemo(
    () => new Set((playbooksData?.playbooks ?? []).map((p) => p.id)),
    [playbooksData],
  );

  // Load fetched source into the store; reset on unmount.
  useEffect(() => {
    if (source && typeof source === "string") {
      store.setToml(source);
      store.markSaved();
      setSavedSource(source);
    }
    return () => store.reset();
  }, [source]); // eslint-disable-line react-hooks/exhaustive-deps

  // Seed an unsaved draft for a brand-new playbook.
  useEffect(() => {
    if (isNew) {
      store.setToml(NEW_PLAYBOOK_STUB);
      setSavedSource(NEW_PLAYBOOK_STUB);
    }
  }, [isNew]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleSave = useCallback(() => {
    if (store.parseError) {
      alert("Fix the TOML parse error before saving.");
      return;
    }
    // The id comes from the TOML body, not the route — changing it renames.
    const targetId = store.parsedDef?.playbook.id?.trim();
    if (!targetId) {
      alert("Set a playbook id (the `id` field under [playbook]) before saving.");
      return;
    }
    const idChanged = targetId !== id;
    if (idChanged && existingIds.has(targetId)) {
      if (!confirm(`A playbook "${targetId}" already exists. Overwrite it?`)) return;
    }
    saveMutation.mutate(
      {
        id: targetId,
        toml: store.tomlSource,
        from: idChanged && !isNew ? id : undefined,
        visibility,
        owner: isNew ? undefined : owner,
      },
      {
        onSuccess: () => {
          store.markSaved();
          setVisibilityOverride(null);
          setSavedSource(store.tomlSource);
          if (idChanged || isNew) navigate(`/playbooks/${targetId}`, { replace: true });
        },
        onError: (err) => {
          alert(`Save failed: ${err instanceof Error ? err.message : String(err)}`);
        },
      },
    );
  }, [id, isNew, owner, store, saveMutation, navigate, existingIds, visibility]);

  // Keyboard shortcut: Cmd/Ctrl+S
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "s") {
        e.preventDefault();
        handleSave();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handleSave]);

  if (!isNew && isLoading) return <PageSkeleton />;

  const viewModes: { value: ViewMode; icon: typeof Code; label: string }[] = [
    { value: "visual", icon: Eye, label: "Visual" },
    { value: "code", icon: Code, label: "Code" },
    { value: "split", icon: Columns2, label: "Split" },
  ];

  const playbookName = store.parsedDef?.playbook.name ?? id ?? "Playbook";

  // Graph + button-editor panel, shared by the visual and split layouts. Click a
  // step to edit its buttons; edits re-serialise the def back into the TOML.
  const graphPane = (
    <div className="relative h-full">
      <ReactFlowProvider>
        <PlaybookBuilder
          definition={store.parsedDef}
          activeStepId={activeStepId}
          onSelectStep={setActiveStepId}
        />
      </ReactFlowProvider>
      {activeStepId && store.parsedDef && (
        <ButtonEditorPanel
          def={store.parsedDef}
          stepId={activeStepId}
          onChange={(d) => store.setToml(playbookDefToToml(d))}
          onClose={() => setActiveStepId(null)}
        />
      )}
    </div>
  );
  const playbookForDialog: PlaybookDef | null =
    !isNew && store.parsedDef
      ? {
          id: store.parsedDef.playbook.id,
          name: store.parsedDef.playbook.name,
          description: store.parsedDef.playbook.description ?? "",
          steps: store.parsedDef.step.length,
        }
      : null;

  return (
    <div className="flex flex-col h-[calc(100vh-3.5rem)]">
      {/* Header bar */}
      <div className="flex items-center justify-between gap-3 px-4 py-2.5 border-b bg-card shrink-0">
        <div className="flex items-center gap-3 min-w-0">
          <Button
            variant="ghost"
            size="icon"
            className="h-8 w-8 shrink-0"
            onClick={() => navigate("/playbooks")}
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div className="min-w-0">
            <h1 className="text-sm font-semibold truncate">{playbookName}</h1>
            {store.parsedDef && (
              <span className="text-xs text-muted-foreground font-mono">
                {store.parsedDef.playbook.id}
              </span>
            )}
          </div>
          <Badge variant="outline" className="text-[10px]">
            interactive
          </Badge>
          {store.isDirty && (
            <Badge variant="warning" className="text-[10px]">
              Unsaved
            </Badge>
          )}
          {store.parseError && (
            <Badge variant="destructive" className="text-[10px]">
              Parse Error
            </Badge>
          )}
        </div>

        <div className="flex items-center gap-2 shrink-0">
          {/* View mode toggle */}
          <div className="flex items-center border rounded-md">
            {viewModes.map(({ value, icon: Icon, label }) => (
              <Button
                key={value}
                variant={store.viewMode === value ? "secondary" : "ghost"}
                size="sm"
                className={`h-7 px-2.5 text-xs ${
                  value === "visual"
                    ? "rounded-r-none"
                    : value === "code"
                      ? "rounded-none"
                      : "rounded-l-none"
                }`}
                onClick={() => store.setViewMode(value)}
              >
                <Icon className="h-3.5 w-3.5 mr-1" />
                {label}
              </Button>
            ))}
          </div>
          <Button
            variant={showDiff ? "secondary" : "outline"}
            size="sm"
            className="h-7 text-xs"
            onClick={() => setShowDiff(!showDiff)}
            disabled={!store.isDirty}
          >
            <Diff className="h-3.5 w-3.5 mr-1" />
            Diff
          </Button>
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            onClick={() => setShowHistory(true)}
            disabled={isNew}
          >
            <History className="h-3.5 w-3.5 mr-1" />
            History
          </Button>
          <VisibilitySelect
            className="h-7 text-xs"
            value={definitionVisibility(visibility)}
            options={visibilityChoices(!isNew && !owner)}
            onChange={(next) => setVisibilityOverride(definitionWord(next))}
          />
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7 text-destructive hover:text-destructive"
            onClick={() => setShowDeleteDialog(true)}
            disabled={isNew}
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
          <Button
            size="sm"
            className="h-7 text-xs"
            onClick={handleSave}
            disabled={
              (!store.isDirty && !visibilityDirty) ||
              !!store.parseError ||
              saveMutation.isPending
            }
          >
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>

      {/* Editor area */}
      <div className="flex-1 min-h-0">
        {showDiff ? (
          <WorkflowDiff original={savedSource} modified={store.tomlSource} />
        ) : store.viewMode === "visual" ? (
          graphPane
        ) : store.viewMode === "code" ? (
          <TomlEditor
            value={store.tomlSource}
            onChange={store.setToml}
            parseError={store.parseError}
            parseErrorLine={store.parseErrorLine}
          />
        ) : (
          <div className="flex h-full">
            <div className="flex-1 border-r min-w-0">{graphPane}</div>
            <div className="flex-1 min-w-0">
              <TomlEditor
                value={store.tomlSource}
                onChange={store.setToml}
                parseError={store.parseError}
                parseErrorLine={store.parseErrorLine}
              />
            </div>
          </div>
        )}
      </div>

      {playbookForDialog && (
        <DeletePlaybookDialog
          playbook={playbookForDialog}
          open={showDeleteDialog}
          onOpenChange={setShowDeleteDialog}
          onDeleted={() => navigate("/playbooks")}
        />
      )}
      {!isNew && id && (
        <RevisionHistoryDialog
          open={showHistory}
          onOpenChange={setShowHistory}
          title={id}
          revisions={revisionsQuery.data?.revisions}
          isLoading={revisionsQuery.isLoading}
          currentContent={store.tomlSource}
          getContent={(rev) =>
            playbooksApi.getRevision(id, rev, owner).then((r) => r.content ?? "")
          }
          onRestore={(rev) =>
            restoreMutation.mutate(
              { id, rev, owner },
              {
                onSuccess: () => setShowHistory(false),
                onError: (err) =>
                  alert(`Restore failed: ${err instanceof Error ? err.message : String(err)}`),
              },
            )
          }
          restorePending={restoreMutation.isPending}
        />
      )}
    </div>
  );
}
