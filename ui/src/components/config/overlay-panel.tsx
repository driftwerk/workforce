import { useState } from "react";
import {
  Layers,
  RotateCcw,
  Download,
  Trash2,
  AlertTriangle,
  CheckCircle,
  Loader2,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  useEffectiveConfig,
  useRevertOverlayKey,
  useClearOverlay,
} from "@/hooks/use-config";
import { configApi } from "@/lib/api/config";

function downloadText(filename: string, content: string) {
  const blob = new Blob([content], { type: "application/toml" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

/**
 * Config overlay panel.
 *
 * workforce.toml is deployed read-only from the infra repo; runtime changes
 * (UI-added agents, runtime knobs) live in the overlay and SHADOW the base —
 * an infra deploy can never change a key while an override is live. This
 * panel makes that visible: every overlay key with provenance, converged
 * detection ("the base caught up — clear the redundant override"), per-key
 * revert-to-base, and the promote flow (export full → commit to infra →
 * deploy → clear overlay).
 */
export function OverlayPanel() {
  const { data: effective, isLoading } = useEffectiveConfig();
  const revertMutation = useRevertOverlayKey();
  const clearMutation = useClearOverlay();
  const [exporting, setExporting] = useState(false);

  async function handleExport(mode: "full" | "fragment") {
    setExporting(true);
    try {
      const content = await configApi.exportConfig(mode);
      downloadText(
        mode === "full" ? "workforce.toml" : "workforce.overlay.toml",
        content,
      );
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setExporting(false);
    }
  }

  function handleRevert(keyPath: string) {
    if (
      !confirm(
        `Revert '${keyPath}' to the base config? The overlay history keeps the removed value.`,
      )
    )
      return;
    revertMutation.mutate(keyPath, {
      onError: (err) =>
        alert(`Revert failed: ${err instanceof Error ? err.message : String(err)}`),
    });
  }

  function handleClear() {
    if (
      !confirm(
        "Clear the entire overlay? Only do this AFTER the exported config has been " +
          "committed to the infra repo and deployed — otherwise these settings revert " +
          "to the old base values.",
      )
    )
      return;
    clearMutation.mutate(undefined, {
      onError: (err) =>
        alert(`Clear failed: ${err instanceof Error ? err.message : String(err)}`),
    });
  }

  if (isLoading) {
    return (
      <div className="flex justify-center py-12">
        <Loader2 className="h-5 w-5 animate-spin" />
      </div>
    );
  }

  const keys = effective?.overlay_keys ?? [];
  const convergedCount = keys.filter((k) => k.converged).length;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-3">
              <Layers className="h-5 w-5 text-muted-foreground" />
              <div>
                <CardTitle className="text-base">Config Overlay</CardTitle>
                <CardDescription>
                  Runtime config changes live here and survive deploys; the base
                  workforce.toml stays infra-owned. Overlay keys shadow the base
                  until reverted or promoted.
                </CardDescription>
              </div>
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={exporting}
                onClick={() => handleExport("full")}
                title="Merged base ⊕ overlay as a complete workforce.toml — commit this to the infra repo to formalise the overrides"
              >
                <Download className="h-3.5 w-3.5 mr-1" />
                Export full config
              </Button>
              <Button
                variant="outline"
                size="sm"
                disabled={exporting || !effective?.overlay_present}
                onClick={() => handleExport("fragment")}
                title="Just the overlay file"
              >
                <Download className="h-3.5 w-3.5 mr-1" />
                Export overlay
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="text-destructive hover:text-destructive"
                disabled={clearMutation.isPending || !effective?.overlay_present}
                onClick={handleClear}
                title="The final promote step: clear AFTER the exported config is deployed as the new base"
              >
                <Trash2 className="h-3.5 w-3.5 mr-1" />
                {clearMutation.isPending ? "Clearing…" : "Clear overlay"}
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {effective?.overlay_error && (
            <div className="flex items-start gap-2 rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm">
              <AlertTriangle className="h-4 w-4 mt-0.5 text-destructive shrink-0" />
              <div>
                <span className="font-medium">Overlay unreadable:</span>{" "}
                {effective.overlay_error} — the hub is running from the base
                config only; overlay-defined agents/settings are inactive until
                the file is fixed.
              </div>
            </div>
          )}
          {(effective?.stripped.length ?? 0) > 0 && (
            <div className="flex items-start gap-2 rounded-md border border-yellow-600/50 bg-yellow-600/10 p-3 text-sm">
              <AlertTriangle className="h-4 w-4 mt-0.5 text-yellow-600 shrink-0" />
              <div>
                <span className="font-medium">Ignored keys (not overlayable):</span>{" "}
                {effective!.stripped.join(", ")} — infra-owned sections can only
                change in workforce.toml via the infra repo.
              </div>
            </div>
          )}

          {keys.length === 0 ? (
            <p className="text-sm text-muted-foreground py-2">
              No overlay overrides — the effective config equals the deployed
              base. Changes made via the UI (e.g. adding an agent) will appear
              here.
            </p>
          ) : (
            <>
              {convergedCount > 0 && (
                <div className="flex items-start gap-2 rounded-md border border-green-600/50 bg-green-600/10 p-3 text-sm">
                  <CheckCircle className="h-4 w-4 mt-0.5 text-green-600 shrink-0" />
                  <div>
                    {convergedCount} override{convergedCount > 1 ? "s have" : " has"}{" "}
                    converged with the base — the deployed config now carries the
                    same value, so the override is redundant. Revert it so future
                    infra changes aren't silently shadowed.
                  </div>
                </div>
              )}
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b text-left text-xs text-muted-foreground">
                    <th className="py-1.5 pr-4 font-medium">Overlay key</th>
                    <th className="py-1.5 pr-4 font-medium">Status</th>
                    <th className="py-1.5 font-medium text-right">Action</th>
                  </tr>
                </thead>
                <tbody>
                  {keys.map((k) => (
                    <tr key={k.key_path} className="border-b last:border-0">
                      <td className="py-1.5 pr-4 font-mono text-xs">{k.key_path}</td>
                      <td className="py-1.5 pr-4">
                        {k.converged ? (
                          <Badge className="text-[10px] bg-green-600">
                            converged — safe to revert
                          </Badge>
                        ) : (
                          <Badge variant="secondary" className="text-[10px]">
                            overrides base
                          </Badge>
                        )}
                      </td>
                      <td className="py-1.5 text-right">
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-6 text-xs"
                          disabled={revertMutation.isPending}
                          onClick={() => handleRevert(k.key_path)}
                        >
                          <RotateCcw className="h-3 w-3 mr-1" />
                          Revert to base
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </>
          )}

          <p className="text-xs text-muted-foreground">
            Promote flow: <span className="font-medium">Export full config</span> →
            commit to the infra repo → deploy →{" "}
            <span className="font-medium">Clear overlay</span>. Restart the hub for
            agent additions/removals to take effect.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
