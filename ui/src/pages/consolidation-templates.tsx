import { useCallback, useState } from "react";
import { useNavigate } from "react-router";
import { Layers, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  useConsolidationTemplates,
  useSaveConsolidationTemplate,
} from "@/hooks/use-consolidation-templates";
import { TemplateCard } from "@/components/consolidation-templates/template-card";
import { DeleteTemplateDialog } from "@/components/consolidation-templates/delete-template-dialog";
import {
  consolidationTemplatesApi,
  type ConsolidationTemplateDef,
} from "@/lib/api/consolidation-templates";
import { useAuthStore } from "@/stores/auth-store";

type OwnerFilter = "all" | "mine" | "global";

export default function ConsolidationTemplatesPage() {
  const { data, isLoading, refetch } = useConsolidationTemplates();
  const navigate = useNavigate();
  const saveMutation = useSaveConsolidationTemplate();

  const [deleteTpl, setDeleteTpl] = useState<ConsolidationTemplateDef | null>(null);
  const [ownerFilter, setOwnerFilter] = useState<OwnerFilter>("all");

  const myId = useAuthStore((s) => s.identity?.id);
  const allTemplates = data?.templates ?? [];
  const templates = allTemplates.filter((t) => {
    if (ownerFilter === "mine") return !!myId && t.owner === myId;
    if (ownerFilter === "global") return t.visibility === "global";
    return true;
  });

  const handleDuplicate = useCallback(
    async (t: ConsolidationTemplateDef) => {
      try {
        const source = await consolidationTemplatesApi.getSource(t.id, t.owner);
        const newId = `${t.id}-copy`;
        saveMutation.mutate(
          { id: newId, json: source },
          {
            onSuccess: () => {
              refetch();
              navigate(`/consolidation-templates/${newId}`);
            },
          },
        );
      } catch (err) {
        alert(`Duplicate failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
    [saveMutation, refetch, navigate],
  );

  const handleNew = useCallback(
    () => navigate("/consolidation-templates/new"),
    [navigate],
  );

  if (isLoading) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-2xl font-bold">Consolidation Templates</h1>
          <p className="text-muted-foreground">
            Named templates that drive a consolidated financial statement across
            multiple Xero entities — a canonical line set plus a per-entity
            account map.
          </p>
        </div>
        <Button onClick={handleNew} disabled={saveMutation.isPending}>
          <Plus className="h-4 w-4 mr-1.5" />
          New Template
        </Button>
      </div>

      {allTemplates.length > 0 && (
        <div className="flex items-center border rounded-md w-fit">
          {(["all", "mine", "global"] as OwnerFilter[]).map((f, i) => (
            <Button
              key={f}
              variant={ownerFilter === f ? "secondary" : "ghost"}
              size="sm"
              className={`h-8 px-2.5 text-xs ${i === 0 ? "rounded-r-none" : i === 1 ? "rounded-none" : "rounded-l-none"}`}
              onClick={() => setOwnerFilter(f)}
            >
              {f === "all" ? "All" : f === "mine" ? "Mine" : "Global"}
            </Button>
          ))}
        </div>
      )}

      {templates.length === 0 ? (
        <EmptyState
          icon={Layers}
          title="No consolidation templates"
          description="Create a template by hand, or upload an .xlsx and refine the parsed result."
          action={{ label: "New Template", onClick: handleNew }}
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {templates.map((t) => (
            <TemplateCard
              key={`${t.owner ?? "global"}/${t.id}`}
              template={t}
              onDuplicate={handleDuplicate}
              onDelete={setDeleteTpl}
            />
          ))}
        </div>
      )}

      <DeleteTemplateDialog
        template={deleteTpl}
        open={deleteTpl !== null}
        onOpenChange={(open) => {
          if (!open) setDeleteTpl(null);
        }}
        onDeleted={() => refetch()}
      />
    </div>
  );
}
