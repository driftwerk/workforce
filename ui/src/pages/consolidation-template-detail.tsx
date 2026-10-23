import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router";
import {
  ArrowLeft,
  Save,
  Trash2,
  ListOrdered,
  Grid3x3,
  Eye,
  History,
} from "lucide-react";
import { VisibilitySelect } from "@/components/sharing/visibility-select";
import {
  definitionVisibility,
  definitionWord,
  visibilityChoices,
} from "@/lib/access";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  useConsolidationTemplates,
  useConsolidationTemplateSource,
  useSaveConsolidationTemplate,
  useConsolidationTemplateRevisions,
  useRestoreConsolidationTemplateRevision,
} from "@/hooks/use-consolidation-templates";
import { consolidationTemplatesApi } from "@/lib/api/consolidation-templates";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { useXeroEntities } from "@/hooks/use-xero-entities";
import {
  useConsolidationTemplateEditorStore,
  serializeTemplate,
} from "@/stores/consolidation-template-editor-store";
import { LinesEditor } from "@/components/consolidation-templates/lines-editor";
import { MappingGrid } from "@/components/consolidation-templates/mapping-grid";
import { TemplatePreview } from "@/components/consolidation-templates/template-preview";
import { TemplateUploadButton } from "@/components/consolidation-templates/template-upload-button";
import { DeleteTemplateDialog } from "@/components/consolidation-templates/delete-template-dialog";

type Tab = "lines" | "mapping" | "preview";

const NEW_TEMPLATE_JSON = serializeTemplate({
  name: "New Template",
  lines: [],
  map: {},
});

export default function ConsolidationTemplateDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const isNew = id === "new";

  const [searchParams] = useSearchParams();
  const { data: templatesData } = useConsolidationTemplates();
  // Owner disambiguates which member's definition this is (two members may
  // share an id); fall back to the list entry, else a system/global def.
  const owner =
    searchParams.get("owner") ??
    templatesData?.templates.find((t) => t.id === id)?.owner ??
    null;
  const { data: source, isLoading } = useConsolidationTemplateSource(
    isNew ? undefined : id,
    owner,
  );
  const saveMutation = useSaveConsolidationTemplate();
  const { data: entities } = useXeroEntities();

  const store = useConsolidationTemplateEditorStore();
  const [tab, setTab] = useState<Tab>("lines");
  const [templateId, setTemplateId] = useState(isNew ? "" : (id ?? ""));
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  // Only fetch revisions while the dialog is open.
  const revisionsQuery = useConsolidationTemplateRevisions(
    showHistory && !isNew ? id : undefined,
    owner,
  );
  const restoreMutation = useRestoreConsolidationTemplateRevision();

  // Visibility (private/global) is storage metadata, not part of the JSON.
  // Track an override the toggle sets; default to the loaded def's visibility,
  // else private (a new draft is private until published).
  const [visibilityOverride, setVisibilityOverride] = useState<string | null>(null);
  const loadedVisibility =
    templatesData?.templates.find(
      (t) => t.id === id && (t.owner ?? null) === owner,
    )?.visibility ?? (isNew || owner ? "private" : "global");
  const visibility = visibilityOverride ?? loadedVisibility;
  const visibilityDirty =
    visibilityOverride !== null && visibilityOverride !== loadedVisibility;
  useEffect(() => setVisibilityOverride(null), [id, owner]);

  const existingIds = useMemo(
    () => new Set((templatesData?.templates ?? []).map((t) => t.id)),
    [templatesData],
  );

  // Load fetched source into the store; reset on unmount.
  useEffect(() => {
    if (source && typeof source === "string") {
      store.load(source);
      setTemplateId(id ?? "");
    }
    return () => store.reset();
  }, [source]); // eslint-disable-line react-hooks/exhaustive-deps

  // Seed an unsaved draft for a brand-new template.
  useEffect(() => {
    if (isNew) {
      store.load(NEW_TEMPLATE_JSON);
      setTemplateId("");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isNew]);

  const idDirty = !isNew && templateId.trim() !== id;
  const isDirty = store.isDirty || idDirty;

  const handleSave = useCallback(() => {
    const targetId = templateId.trim();
    if (!targetId) {
      alert("Set a template id before saving.");
      return;
    }
    const idChanged = !isNew && targetId !== id;
    if ((idChanged || isNew) && targetId !== id && existingIds.has(targetId)) {
      if (!confirm(`A template "${targetId}" already exists. Overwrite it?`)) return;
    }
    saveMutation.mutate(
      {
        id: targetId,
        json: serializeTemplate(store.template),
        from: idChanged ? id : undefined,
        visibility,
      },
      {
        onSuccess: () => {
          store.markSaved();
          setVisibilityOverride(null);
          if (idChanged || isNew) {
            navigate(`/consolidation-templates/${targetId}`, { replace: true });
          }
        },
        onError: (err) => {
          alert(`Save failed: ${err instanceof Error ? err.message : String(err)}`);
        },
      },
    );
  }, [id, isNew, templateId, store, saveMutation, navigate, existingIds, visibility]);

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

  const entityList = entities ?? [];
  const templateName = store.template.name?.trim() || templateId || "Template";

  const tabs: { value: Tab; icon: typeof ListOrdered; label: string }[] = [
    { value: "lines", icon: ListOrdered, label: "Lines" },
    { value: "mapping", icon: Grid3x3, label: "Mapping" },
    { value: "preview", icon: Eye, label: "Preview" },
  ];

  return (
    <div className="flex flex-col h-[calc(100vh-3.5rem)]">
      {/* Header bar */}
      <div className="flex items-center justify-between gap-3 px-4 py-2.5 border-b bg-card shrink-0">
        <div className="flex items-center gap-3 min-w-0">
          <Button
            variant="ghost"
            size="icon"
            className="h-8 w-8 shrink-0"
            onClick={() => navigate("/consolidation-templates")}
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div className="min-w-0">
            <h1 className="text-sm font-semibold truncate">{templateName}</h1>
            <div className="flex items-center gap-1.5">
              <span className="text-[11px] text-muted-foreground">id</span>
              <Input
                className="h-6 w-44 text-xs font-mono px-2"
                placeholder="template-id"
                value={templateId}
                onChange={(e) => setTemplateId(e.target.value)}
              />
            </div>
          </div>
          <Badge variant="outline" className="text-[10px]">
            consolidation
          </Badge>
          {isDirty && (
            <Badge variant="warning" className="text-[10px]">
              Unsaved
            </Badge>
          )}
        </div>

        <div className="flex items-center gap-2 shrink-0">
          <TemplateUploadButton />
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
            disabled={(!isDirty && !visibilityDirty) || saveMutation.isPending}
          >
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>

      {/* Name + tab strip */}
      <div className="flex items-center justify-between gap-3 px-4 py-2 border-b shrink-0">
        <Input
          className="h-8 max-w-sm text-sm"
          placeholder="Template name"
          value={store.template.name ?? ""}
          onChange={(e) => store.setName(e.target.value)}
        />
        <div className="flex items-center border rounded-md">
          {tabs.map(({ value, icon: Icon, label }, i) => (
            <Button
              key={value}
              variant={tab === value ? "secondary" : "ghost"}
              size="sm"
              className={`h-7 px-2.5 text-xs ${i === 0 ? "rounded-r-none" : i === tabs.length - 1 ? "rounded-l-none" : "rounded-none"}`}
              onClick={() => setTab(value)}
            >
              <Icon className="h-3.5 w-3.5 mr-1" />
              {label}
            </Button>
          ))}
        </div>
      </div>

      {/* Editor body */}
      <div className="flex-1 min-h-0 overflow-y-auto p-4">
        {tab === "lines" ? (
          <LinesEditor />
        ) : tab === "mapping" ? (
          <MappingGrid entities={entityList} />
        ) : (
          <TemplatePreview entities={entityList} />
        )}
      </div>

      {!isNew && id && (
        <DeleteTemplateDialog
          template={{ id, name: templateName }}
          open={showDeleteDialog}
          onOpenChange={setShowDeleteDialog}
          onDeleted={() => navigate("/consolidation-templates")}
        />
      )}
      {!isNew && id && (
        <RevisionHistoryDialog
          open={showHistory}
          onOpenChange={setShowHistory}
          title={id}
          revisions={revisionsQuery.data?.revisions}
          isLoading={revisionsQuery.isLoading}
          currentContent={serializeTemplate(store.template)}
          getContent={(rev) =>
            consolidationTemplatesApi
              .getRevision(id, rev, owner)
              .then((r) => r.content ?? "")
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
