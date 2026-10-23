import { useState, useEffect, useCallback } from "react";
import { Save, Loader2, Lock } from "lucide-react";
import { parse } from "smol-toml";
import { Button } from "@/components/ui/button";
import { TomlEditor } from "@/components/workflows/toml-editor";
import { OverlayPanel } from "@/components/config/overlay-panel";
import { useRawConfig, useConfigOverlay, useSaveOverlay } from "@/hooks/use-config";

/** Config prefixes this editor can override — the OVERLAYABLE_PREFIXES from
 *  crates/common/src/config_overlay.rs, minus the agent keys (agent entries
 *  are owned by the Agents tab and preserved on save, not edited here). */
const OVERRIDABLE =
  "llm.routing, llm.model_preferences, llm.equivalences, mcp.enabled, environments, games";

/** Server errors arrive as the response body, which is `{"error": "..."}`
 *  JSON for these handlers. Pull the message out; fall back to the raw text. */
function errorMessage(err: unknown): string {
  const raw = err instanceof Error ? err.message : String(err);
  try {
    const parsed = JSON.parse(raw) as { error?: unknown };
    if (typeof parsed.error === "string") return parsed.error;
  } catch {
    /* not JSON — use the raw text */
  }
  return raw;
}

/**
 * Settings → Config tab. The base workforce.toml is deployed read-only from
 * the infra repo (a deploy overwrites it), so runtime changes go to the
 * overlay instead: an allowlisted fragment on the persistent volume that
 * shadows the base at load. Base shown read-only on the left; the editable
 * overlay on the right saves to PUT /api/config/overlay. The OverlayPanel
 * below carries provenance, revert-to-base, export, and the promote flow.
 */
export function ConfigOverlayEditor() {
  const { data: baseToml, isLoading: baseLoading } = useRawConfig();
  const { data: overlayToml, isLoading: overlayLoading } = useConfigOverlay();
  const saveOverlay = useSaveOverlay();

  const [draft, setDraft] = useState("");
  const [loadedFrom, setLoadedFrom] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [parseError, setParseError] = useState<string | null>(null);
  const [parseErrorLine, setParseErrorLine] = useState<number | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  /* Seed from the loaded overlay, and re-seed when it changes upstream
     (revert/clear/save normalisation) — but never clobber unsaved edits. */
  useEffect(() => {
    if (overlayToml === undefined) return;
    if (!dirty && overlayToml !== loadedFrom) {
      setDraft(overlayToml);
      setLoadedFrom(overlayToml);
    }
  }, [overlayToml, dirty, loadedFrom]);

  const handleChange = useCallback((val: string) => {
    setDraft(val);
    setDirty(true);
    setSaveError(null);
    try {
      parse(val); // an empty overlay is valid
      setParseError(null);
      setParseErrorLine(null);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "Invalid TOML";
      setParseError(msg);
      const m = msg.match(/line (\d+)/i);
      setParseErrorLine(m ? parseInt(m[1]!, 10) : null);
    }
  }, []);

  function handleSave() {
    setSaveError(null);
    saveOverlay.mutate(draft, {
      onSuccess: () => {
        setDirty(false);
        setLoadedFrom(draft);
      },
      onError: (err) => setSaveError(errorMessage(err)),
    });
  }

  // Gate on BOTH queries: seeding the editor before the overlay resolves
  // would let an edit in the fetch-race window block the real overlay from
  // ever seeding, and Save would then overwrite it with a partial draft.
  if (baseLoading || overlayLoading) {
    return (
      <div className="flex justify-center py-12">
        <Loader2 className="h-5 w-5 animate-spin" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-4" style={{ minHeight: 480 }}>
        {/* Base — read-only */}
        <div className="flex flex-col space-y-2">
          <div className="flex items-center gap-1.5 h-8">
            <Lock className="h-3.5 w-3.5 text-muted-foreground" />
            <span className="text-sm font-medium">workforce.toml</span>
            <span className="text-xs text-muted-foreground">— base, read-only</span>
          </div>
          <div className="flex-1 min-h-0 border rounded-lg overflow-hidden">
            <TomlEditor
              value={typeof baseToml === "string" ? baseToml : ""}
              onChange={() => {}}
              parseError={null}
              parseErrorLine={null}
              readOnly
            />
          </div>
          <p className="text-xs text-muted-foreground">
            Deployed from the infra repo and read-only — a deploy overwrites it.
            Add runtime overrides on the right.
          </p>
        </div>

        {/* Overlay — editable */}
        <div className="flex flex-col space-y-2">
          <div className="flex items-center justify-between h-8">
            <div>
              <span className="text-sm font-medium">Overlay</span>
              <span className="text-xs text-muted-foreground"> — runtime overrides</span>
            </div>
            <Button
              size="sm"
              disabled={!dirty || !!parseError || saveOverlay.isPending}
              onClick={handleSave}
            >
              <Save className="h-3.5 w-3.5 mr-1" />
              {saveOverlay.isPending ? "Saving…" : "Save overlay"}
            </Button>
          </div>
          <div className="flex-1 min-h-0 border rounded-lg overflow-hidden">
            <TomlEditor
              value={draft}
              onChange={handleChange}
              parseError={parseError}
              parseErrorLine={parseErrorLine}
            />
          </div>
          {saveError ? (
            <div className="rounded-md border border-destructive/50 bg-destructive/10 p-2 text-xs text-destructive">
              {saveError}
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              Overridable: <span className="font-mono">{OVERRIDABLE}</span>. Other
              sections are infra-owned. Agent entries are managed in the Agents
              tab — edits to <span className="font-mono">[[agents]]</span> here
              aren't saved. <span className="font-mono">environments.enabled = false</span> is
              only a kill switch that turns environments off on every agent; each agent's
              environments live on its Environments tab. Changes apply on the next hub restart.
            </p>
          )}
        </div>
      </div>

      <OverlayPanel />
    </div>
  );
}
