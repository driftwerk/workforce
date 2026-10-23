import { Download, Loader2, Check, AlertTriangle, Package, RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useSkillCatalog, useInstallFromCatalog } from "@/hooks/use-skills";
import type { CatalogSkillView } from "@/lib/api/skills";
import { apiErrorMessage } from "@/lib/api-error";
import { CATALOG_ACTION_LABEL, catalogAction, installNote } from "@/lib/skills/catalog";
import { cn } from "@/lib/utils";

export function SkillCatalog() {
  const { data, isLoading, error } = useSkillCatalog();
  const catalogs = data?.catalogs ?? [];

  // Opt-in operator feature: stay hidden unless a catalog is configured — but a
  // real fetch error is surfaced rather than silently swallowed.
  if (!isLoading && !error && catalogs.length === 0) return null;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Package className="h-4 w-4 text-muted-foreground" />
        <h2 className="text-sm font-semibold">Install from catalog</h2>
      </div>
      {isLoading ? (
        <div className="flex items-center justify-center h-24">
          <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
        </div>
      ) : error ? (
        <div className="text-xs text-destructive">
          Failed to load catalog: {apiErrorMessage(error)}
        </div>
      ) : (
        catalogs.map((cat) => (
          <div key={cat.name} className="rounded-lg border bg-card">
            <div className="flex items-center justify-between gap-2 border-b px-4 py-2">
              <span className="text-sm font-medium">{cat.name}</span>
              <span className="text-xs text-muted-foreground truncate">{cat.repo}</span>
            </div>
            {cat.error ? (
              <div className="flex items-center gap-2 px-4 py-3 text-xs text-destructive">
                <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
                {cat.error}
              </div>
            ) : cat.skills.length === 0 ? (
              <div className="px-4 py-3 text-xs text-muted-foreground">
                No skills listed.
              </div>
            ) : (
              <ul className="divide-y">
                {cat.skills.map((skill) => (
                  <CatalogRow key={`${cat.name}/${skill.name}`} skill={skill} />
                ))}
              </ul>
            )}
          </div>
        ))
      )}
    </div>
  );
}

function CatalogRow({ skill }: { skill: CatalogSkillView }) {
  const install = useInstallFromCatalog();
  const action = catalogAction(skill);
  const ActionIcon = action === "install" ? Download : RefreshCw;

  return (
    <li className="flex items-start justify-between gap-3 px-4 py-3">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium truncate">{skill.name}</span>
          <Badge variant="outline" className="text-[10px] shrink-0">
            {skill.kind}
          </Badge>
          {skill.version && (
            <span className="text-[10px] text-muted-foreground">v{skill.version}</span>
          )}
          {skill.built_from_source && (
            <Badge variant="outline" className="text-[10px] shrink-0">
              builds on install
            </Badge>
          )}
        </div>
        {skill.description && (
          <p className="text-xs text-muted-foreground mt-1 line-clamp-2">
            {skill.description}
          </p>
        )}
        {skill.requires_bins && skill.requires_bins.length > 0 && (
          <p className="text-[10px] text-muted-foreground mt-1">
            Requires: {skill.requires_bins.join(", ")}
          </p>
        )}
        {install.isError && (
          <p className="text-[10px] text-destructive mt-1">
            {apiErrorMessage(install.error)}
          </p>
        )}
        {install.data && (
          <p
            className={cn(
              "text-[10px] mt-1",
              install.data.active ? "text-muted-foreground" : "text-warning",
            )}
          >
            {installNote(install.data)}
          </p>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        {skill.installed && (
          <Badge variant="secondary" className="gap-1">
            <Check aria-hidden className="h-3 w-3" />
            {skill.installed_version ? `Installed v${skill.installed_version}` : "Installed"}
          </Badge>
        )}
        <Button
          size="sm"
          variant="outline"
          disabled={install.isPending}
          onClick={() =>
            install.mutate({ catalog: skill.catalog, name: skill.name })
          }
        >
          {install.isPending ? (
            <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <ActionIcon aria-hidden className="h-3.5 w-3.5" />
          )}
          {CATALOG_ACTION_LABEL[action]}
        </Button>
      </div>
    </li>
  );
}
