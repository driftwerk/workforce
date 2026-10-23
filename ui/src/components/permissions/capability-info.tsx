import { Info } from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import type { CapabilityCatalogEntry } from "@/lib/api/permissions";

/**
 * Info icon + Radix tooltip describing one capability — its plain-language
 * description and the tools it governs. Joined to the catalog by capability
 * `key`; renders nothing when the catalog hasn't loaded the entry (so the
 * matrix stays legible mid-deploy / for unknown keys).
 */
export function CapabilityInfo({
  entry,
}: {
  entry: CapabilityCatalogEntry | undefined;
}) {
  if (!entry) return null;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={`What is "${entry.label}"?`}
          className="text-muted-foreground hover:text-foreground"
        >
          <Info className="h-3.5 w-3.5" />
        </button>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs space-y-2">
        <p className="text-xs leading-relaxed">{entry.description}</p>
        {entry.tools.length > 0 && (
          <div className="space-y-1">
            <p className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
              Governs
            </p>
            <div className="flex flex-wrap gap-1">
              {entry.tools.map((t) => (
                <code
                  key={t}
                  className="rounded bg-muted px-1 py-0.5 font-mono text-[10px]"
                >
                  {t}
                </code>
              ))}
            </div>
          </div>
        )}
      </TooltipContent>
    </Tooltip>
  );
}
