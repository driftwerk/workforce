import { useMemo } from "react";
import { Loader2, ShieldCheck } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useMyCapabilities, useCapabilityCatalog } from "@/hooks/use-permissions";
import { CapabilityInfo } from "@/components/permissions/capability-info";
import {
  DecisionBadge,
  humanizeKey,
  roleLabelFallback,
} from "@/components/permissions/labels";
import type {
  CapabilityCatalogEntry,
  CapabilityDecisionValue,
} from "@/lib/api/permissions";

const DECISION_DESCRIPTION: Record<CapabilityDecisionValue, string> = {
  allow: "You can do this without prompting.",
  ask: "You can do this, but it prompts for confirmation.",
  deny: "You can't do this.",
};

/** Bucket for capabilities the catalog doesn't describe (e.g. deploy skew). */
const UNCATALOGUED_CATEGORY = "other";

export default function MyPermissionsPage() {
  const mine = useMyCapabilities();
  const catalog = useCapabilityCatalog();

  const catalogByKey = useMemo(() => {
    const m = new Map<string, CapabilityCatalogEntry>();
    for (const c of catalog.data?.capabilities ?? []) m.set(c.key, c);
    return m;
  }, [catalog.data]);

  // Group MY capabilities by category. The user's own decisions are the source
  // of truth, so a capability I hold is never hidden just because the catalog
  // lacks an entry for it (deploy skew) — it falls into an "Other" bucket.
  const groups = useMemo(() => {
    const byCategory = new Map<
      string,
      { capability: string; decision: CapabilityDecisionValue }[]
    >();
    for (const { capability, decision } of mine.data?.capabilities ?? []) {
      const category = catalogByKey.get(capability)?.category ?? UNCATALOGUED_CATEGORY;
      const list = byCategory.get(category) ?? [];
      list.push({ capability, decision });
      byCategory.set(category, list);
    }
    return [...byCategory.entries()];
  }, [mine.data, catalogByKey]);

  const loading = mine.isLoading || catalog.isLoading;
  const error = mine.error ?? catalog.error;

  return (
    <div className="mx-auto max-w-3xl space-y-6 p-6">
      <div className="flex items-center gap-2">
        <ShieldCheck className="h-5 w-5" />
        <h1 className="text-xl font-semibold">My Permissions</h1>
      </div>
      <p className="text-sm text-muted-foreground">
        What you — and the agents acting on your behalf — are allowed to do. This
        is derived from your role in the always-on capability matrix. To change
        it, ask a system administrator.
      </p>

      {loading ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Loading your permissions…
        </div>
      ) : error ? (
        <Card>
          <CardContent className="p-4 text-sm text-destructive">
            Failed to load your permissions:{" "}
            {error instanceof Error ? error.message : "Unknown error"}
          </CardContent>
        </Card>
      ) : (
        <>
          <Card>
            <CardContent className="flex flex-wrap items-center gap-3 p-4">
              <span className="text-sm font-medium">{mine.data?.display_name}</span>
              <span className="text-muted-foreground">·</span>
              <span className="text-sm text-muted-foreground">Your role</span>
              <Badge variant="secondary">
                {roleLabelFallback(mine.data?.role ?? "")}
              </Badge>
            </CardContent>
          </Card>

          {groups.length === 0 ? (
            <Card>
              <CardContent className="p-6 text-center text-sm text-muted-foreground">
                No capabilities to show.
              </CardContent>
            </Card>
          ) : (
            <div className="space-y-4">
              {groups.map(([category, caps]) => (
                <Card key={category}>
                  <CardHeader className="pb-2">
                    <CardTitle className="text-base">
                      {humanizeKey(category)}
                    </CardTitle>
                  </CardHeader>
                  <CardContent className="space-y-3">
                    {caps.map(({ capability, decision }) => {
                      const entry = catalogByKey.get(capability);
                      return (
                        <div
                          key={capability}
                          className="flex items-start justify-between gap-3 border-b pb-3 last:border-0 last:pb-0"
                        >
                          <div className="min-w-0 space-y-0.5">
                            <div className="flex items-center gap-1.5">
                              <span className="text-sm font-medium">
                                {entry?.label ?? humanizeKey(capability)}
                              </span>
                              <CapabilityInfo entry={entry} />
                            </div>
                            <p className="text-xs text-muted-foreground">
                              {entry?.description ??
                                DECISION_DESCRIPTION[decision]}
                            </p>
                          </div>
                          <DecisionBadge decision={decision} />
                        </div>
                      );
                    })}
                  </CardContent>
                </Card>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}
