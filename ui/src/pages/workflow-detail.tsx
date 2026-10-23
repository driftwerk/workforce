import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router";
import { ReactFlowProvider } from "@xyflow/react";
import { ArrowLeft, Save, Code, Eye, Columns2, Diff, Play, Trash2, History, UserRound, Users } from "lucide-react";
import { VisibilitySelect } from "@/components/sharing/visibility-select";
import {
  definitionVisibility,
  definitionWord,
  visibilityChoices,
} from "@/lib/access";
import { useAuthStore } from "@/stores/auth-store";
import type { RunAs } from "@/lib/api/workflows";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  useWorkflows,
  useWorkflowSource,
  useSaveWorkflow,
  useWorkflowInstancesForDef,
  useWorkflowRevisions,
  useRestoreWorkflowRevision,
} from "@/hooks/use-workflows";
import { workflowsApi } from "@/lib/api/workflows";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { useWorkflowEditorStore, type ViewMode } from "@/stores/workflow-editor-store";
import { PipelineBuilder } from "@/components/workflows/pipeline-builder";
import { TomlEditor } from "@/components/workflows/toml-editor";
import { VariablesPanel } from "@/components/workflows/variables-panel";
import { upsertVariablesBlock } from "@/lib/workflows/variables-toml";
import { WorkflowDiff } from "@/components/workflows/workflow-diff";
import { QuickStartWizard } from "@/components/workflows/quick-start-wizard";
import { DeleteWorkflowDialog } from "@/components/workflows/delete-workflow-dialog";
import { InstanceTable } from "@/components/workflows/instance-table";
import { InstanceDetail } from "@/components/workflows/instance-detail";
import { TriggerUrlsPanel } from "@/components/workflows/trigger-urls-panel";
import type { WorkflowDef } from "@/lib/api/workflows";

// Starter TOML for an unsaved draft; the placeholder id is meant to be replaced.
const NEW_WORKFLOW_STUB = `[workflow]
id = "my-workflow"
name = "New Workflow"
description = ""

[[step]]
id = "start"
name = "First Step"
task = "Describe what this step should do."
`;

export default function WorkflowDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  // `/workflows/new` is an unsaved draft — don't fetch a definition for it.
  const isNew = id === "new";
  const { data: workflowsData } = useWorkflows();
  // Owner disambiguates which owner's definition this is (two members may share
  // a bare id). Prefer the explicit query param; fall back to the list entry.
  const owner =
    searchParams.get("owner") ??
    workflowsData?.workflows.find((w) => w.id === id)?.owner ??
    null;
  const { data: source, isLoading } = useWorkflowSource(isNew ? undefined : id, owner);
  const saveMutation = useSaveWorkflow();
  const existingIds = useMemo(
    () => new Set((workflowsData?.workflows ?? []).map((w) => w.id)),
    [workflowsData],
  );

  const store = useWorkflowEditorStore();
  const [savedSource, setSavedSource] = useState("");
  const [showDiff, setShowDiff] = useState(false);
  const [showRunWizard, setShowRunWizard] = useState(false);
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  // Only fetch revisions while the dialog is open.
  const revisionsQuery = useWorkflowRevisions(showHistory && !isNew ? id : undefined, owner);
  const restoreMutation = useRestoreWorkflowRevision();
  const [instanceStatusFilter, setInstanceStatusFilter] = useState<string | undefined>();

  // Visibility (private/global) is storage metadata, not part of the TOML. Track
  // an override the user picks; default to the loaded value (private for a new
  // draft). Reset the override when navigating to a different definition.
  const [visibilityOverride, setVisibilityOverride] = useState<string | null>(null);
  const loadedEntry = workflowsData?.workflows.find(
    (w) => w.id === id && (w.owner ?? null) === owner,
  );
  // The list row already carries where this workflow's HTTP triggers answer,
  // so the Triggers tab needs no fetch of its own.
  const triggerUrls = loadedEntry?.trigger_urls ?? [];
  const loadedVisibility = loadedEntry?.visibility ?? "private";
  const visibility = visibilityOverride ?? loadedVisibility;
  useEffect(() => setVisibilityOverride(null), [id, owner]);
  const visibilityDirty = visibilityOverride !== null && visibilityOverride !== loadedVisibility;

  // Run identity is save-time storage metadata like visibility; only the saver
  // can name themselves, so the choice is "owner" or "me".
  const myId = useAuthStore((s) => s.identity?.id);
  const [runAsOverride, setRunAsOverride] = useState<RunAs | null>(null);
  useEffect(() => setRunAsOverride(null), [id, owner]);
  const loadedRunAs: string =
    loadedEntry?.run_as == null ? "owner" : loadedEntry.run_as === myId ? "me" : loadedEntry.run_as;
  const runAs = runAsOverride ?? loadedRunAs;
  const runAsDirty = runAsOverride !== null && runAsOverride !== loadedRunAs;
  const runAsLabel =
    runAs === "owner" ? "Runs as owner" : runAs === "me" ? "Runs as you" : `Runs as ${runAs.slice(0, 8)}`;
  const iAmOwner = !!myId && myId === owner;
  const runsAsSomeoneElse = runAs !== "owner" && runAs !== "me";
  // One click changes it, a second click abandons the change.
  const toggleRunAs = () =>
    setRunAsOverride(runAsOverride ? null : runAs === "me" || iAmOwner ? "owner" : "me");
  const runAsTitle =
    runAs === "me"
      ? "Webhook and scheduled runs use your access — click to hand them back to the owner"
      : runsAsSomeoneElse
        ? `Webhook and scheduled runs use ${runAs}'s access — click to ${iAmOwner ? "take them back" : "run them as you"}`
        : iAmOwner
          ? "Webhook and scheduled runs use your access as the owner"
          : "Webhook and scheduled runs use the owner's access — click to run them as you";

  const selectedInstanceId = searchParams.get("instance") ?? null;

  const { data: instanceData } = useWorkflowInstancesForDef(id);
  const instances = instanceData?.instances ?? [];
  const filteredInstances = instanceStatusFilter
    ? instances.filter((i) => i.status === instanceStatusFilter)
    : instances;
  const runningCount = instances.filter(
    (i) => i.status === "running" || i.status === "paused",
  ).length;

  // Load TOML source into store when fetched
  useEffect(() => {
    if (source && typeof source === "string") {
      store.setToml(source);
      store.markSaved();
      setSavedSource(source);
    }
    return () => {
      store.reset();
    };
  }, [source]); // eslint-disable-line react-hooks/exhaustive-deps

  // Seed an unsaved draft for a brand-new workflow.
  useEffect(() => {
    if (isNew) {
      store.setToml(NEW_WORKFLOW_STUB);
    }
  }, [isNew]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleSave = useCallback(() => {
    if (store.parseError) {
      alert("Fix the TOML parse error before saving.");
      return;
    }
    // The id comes from the TOML body, not the route — a new draft saves under
    // the id the user chose, and changing the id renames the workflow.
    const targetId = store.parsedDef?.workflow.id?.trim();
    if (!targetId) {
      alert("Set a workflow id (the `id` field under [workflow]) before saving.");
      return;
    }
    const idChanged = targetId !== id;
    if (idChanged && existingIds.has(targetId)) {
      if (!confirm(`A workflow "${targetId}" already exists. Overwrite it?`)) return;
    }
    saveMutation.mutate(
      {
        id: targetId,
        toml: store.tomlSource,
        from: idChanged && !isNew ? id : undefined,
        visibility,
        owner: isNew ? undefined : owner,
        runAs: runAsOverride ?? undefined,
      },
      {
        onSuccess: () => {
          store.markSaved();
          setVisibilityOverride(null);
          setRunAsOverride(null);
          setSavedSource(store.tomlSource);
          if (idChanged) navigate(`/workflows/${targetId}`, { replace: true });
        },
        onError: (err) => {
          alert(`Save failed: ${err instanceof Error ? err.message : String(err)}`);
        },
      },
    );
  }, [id, isNew, owner, store, saveMutation, navigate, existingIds, visibility, runAsOverride]);

  // Keyboard shortcut: Cmd+S / Ctrl+S
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

  const handleSelectInstance = (instanceId: string) => {
    setSearchParams({ instance: instanceId });
  };

  const handleBackFromInstance = () => {
    setSearchParams({});
  };

  if (isLoading) return <PageSkeleton />;

  // If an instance is selected, show instance detail view
  if (selectedInstanceId) {
    return (
      <div className="flex flex-col h-[calc(100vh-3.5rem)]">
        <InstanceDetail
          instanceId={selectedInstanceId}
          onBack={handleBackFromInstance}
        />
      </div>
    );
  }

  const viewModes: { value: ViewMode; icon: typeof Code; label: string }[] = [
    { value: "visual", icon: Eye, label: "Visual" },
    { value: "code", icon: Code, label: "Code" },
    { value: "split", icon: Columns2, label: "Split" },
  ];

  const workflowName = store.parsedDef?.workflow.name ?? id ?? "Workflow";

  // Part of the definition (unlike visibility), so it is edited into the TOML;
  // absent means on, and switching it back on removes the line.
  const steerableByOthers = store.parsedDef?.workflow.steerable_by_others !== false;
  const toggleSteering = () => {
    if (!store.parsedDef) return;
    const { steerable_by_others: _, ...meta } = store.parsedDef.workflow;
    store.setFromDef({
      ...store.parsedDef,
      workflow: steerableByOthers ? { ...meta, steerable_by_others: false } : meta,
    });
  };

  const workflowForWizard: WorkflowDef | null = store.parsedDef
    ? {
        id: store.parsedDef.workflow.id,
        name: store.parsedDef.workflow.name,
        description: store.parsedDef.workflow.description ?? "",
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
            onClick={() => navigate("/workflows")}
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div className="min-w-0">
            <h1 className="text-sm font-semibold truncate">{workflowName}</h1>
            {store.parsedDef && (
              <span className="text-xs text-muted-foreground font-mono">
                {store.parsedDef.workflow.id}
              </span>
            )}
          </div>
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
          {loadedEntry?.automated_runs_blocked && (
            <Badge variant="warning" className="text-[10px]" title={loadedEntry.automated_runs_blocked}>
              Automation paused
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
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            onClick={toggleSteering}
            disabled={!store.parsedDef}
            title={
              steerableByOthers
                ? "Anyone this workflow is shared with may steer a run in progress (approve, reject, resume, skip, stop). The run still acts as the person who started it. Click to allow only the starter and administrators."
                : "Only the person who started a run, and administrators, may steer it. Click to let anyone this workflow is shared with steer runs too."
            }
          >
            <Users className="h-3.5 w-3.5 mr-1" />
            {steerableByOthers ? "Others can steer runs" : "Only the starter steers runs"}
          </Button>
          {owner && !isNew && (
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-xs"
              onClick={toggleRunAs}
              disabled={iAmOwner && runAs === "owner" && !runAsOverride}
              title={runAsTitle}
            >
              <UserRound className="h-3.5 w-3.5 mr-1" />
              {runAsLabel}
            </Button>
          )}
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            onClick={() => setShowRunWizard(true)}
            disabled={!workflowForWizard}
          >
            <Play className="h-3.5 w-3.5 mr-1" />
            Run
          </Button>
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
              (!store.isDirty && !visibilityDirty && !runAsDirty) ||
              !!store.parseError ||
              saveMutation.isPending
            }
          >
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>

      {/* Tabs: Editor | Instances */}
      <Tabs defaultValue="editor" className="flex flex-col flex-1 min-h-0">
        <TabsList className="h-auto bg-transparent rounded-none border-b px-4 justify-start gap-0 p-0">
          <TabsTrigger
            value="editor"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Editor
          </TabsTrigger>
          <TabsTrigger
            value="variables"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Variables
            {(store.parsedDef?.variables?.length ?? 0) > 0 && (
              <Badge variant="secondary" className="ml-1.5 text-[9px] h-4 px-1">
                {store.parsedDef!.variables!.length}
              </Badge>
            )}
          </TabsTrigger>
          {triggerUrls.length > 0 && (
            <TabsTrigger
              value="triggers"
              className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
            >
              Triggers
              <Badge variant="secondary" className="ml-1.5 text-[9px] h-4 px-1">
                {triggerUrls.length}
              </Badge>
            </TabsTrigger>
          )}
          <TabsTrigger
            value="instances"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Instances
            {runningCount > 0 && (
              <Badge variant="default" className="ml-1.5 text-[9px] h-4 px-1">
                {runningCount}
              </Badge>
            )}
          </TabsTrigger>
        </TabsList>

        <TabsContent value="editor" className="flex-1 min-h-0 m-0 h-full">
          <div className="h-full">
            {showDiff ? (
              <WorkflowDiff original={savedSource} modified={store.tomlSource} />
            ) : store.viewMode === "visual" ? (
              <ReactFlowProvider>
                <PipelineBuilder definition={store.parsedDef} />
              </ReactFlowProvider>
            ) : store.viewMode === "code" ? (
              <TomlEditor
                value={store.tomlSource}
                onChange={store.setToml}
                parseError={store.parseError}
                parseErrorLine={store.parseErrorLine}
              />
            ) : (
              /* Split view */
              <div className="flex h-full">
                <div className="flex-1 border-r min-w-0">
                  <ReactFlowProvider>
                    <PipelineBuilder definition={store.parsedDef} />
                  </ReactFlowProvider>
                </div>
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
        </TabsContent>

        <TabsContent value="variables" className="flex-1 min-h-0 m-0 overflow-y-auto">
          <VariablesPanel
            variables={store.parsedDef?.variables ?? []}
            onChange={(vars) => store.setToml(upsertVariablesBlock(store.tomlSource, vars))}
          />
        </TabsContent>

        {triggerUrls.length > 0 && (
          <TabsContent value="triggers" className="flex-1 min-h-0 m-0 overflow-y-auto">
            <TriggerUrlsPanel triggers={triggerUrls} />
          </TabsContent>
        )}

        <TabsContent value="instances" className="flex-1 min-h-0 m-0 overflow-y-auto p-4">
          <InstanceTable
            instances={filteredInstances}
            onStatusFilter={setInstanceStatusFilter}
            activeStatus={instanceStatusFilter}
            onSelectInstance={handleSelectInstance}
          />
        </TabsContent>
      </Tabs>

      {/* Dialogs */}
      {workflowForWizard && (
        <QuickStartWizard
          workflow={workflowForWizard}
          open={showRunWizard}
          onOpenChange={setShowRunWizard}
        />
      )}
      {workflowForWizard && (
        <DeleteWorkflowDialog
          workflow={workflowForWizard}
          open={showDeleteDialog}
          onOpenChange={setShowDeleteDialog}
          onDeleted={() => navigate("/workflows")}
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
            workflowsApi.getRevision(id, rev, owner).then((r) => r.content ?? "")
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
