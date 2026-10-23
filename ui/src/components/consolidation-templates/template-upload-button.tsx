import { useRef, useState } from "react";
import { Upload, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { consolidationTemplatesApi } from "@/lib/api/consolidation-templates";
import { useConsolidationTemplateEditorStore } from "@/stores/consolidation-template-editor-store";

/**
 * Upload an `.xlsx`, stage it for an `artifact_id`, parse it server-side, and
 * load the resulting draft template into the editor (replacing the current
 * lines + map).
 */
export function TemplateUploadButton() {
  const inputRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);
  const replace = useConsolidationTemplateEditorStore((s) => s.replace);
  const currentName = useConsolidationTemplateEditorStore((s) => s.template.name);

  const handleFile = async (file: File) => {
    setBusy(true);
    try {
      const { artifact_id } = await consolidationTemplatesApi.uploadXlsx(file);
      const parsed = await consolidationTemplatesApi.parseXlsx(artifact_id);
      // Keep the existing name if the parsed template didn't carry one.
      replace({ ...parsed, name: parsed.name?.trim() || currentName });
    } catch (err) {
      alert(`Upload failed: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <input
        ref={inputRef}
        type="file"
        accept=".xlsx,application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        className="hidden"
        onChange={(e) => {
          const file = e.target.files?.[0];
          if (file) handleFile(file);
          e.target.value = "";
        }}
      />
      <Button
        variant="outline"
        size="sm"
        className="h-7 text-xs"
        disabled={busy}
        onClick={() => inputRef.current?.click()}
      >
        {busy ? (
          <Loader2 className="h-3.5 w-3.5 mr-1 animate-spin" />
        ) : (
          <Upload className="h-3.5 w-3.5 mr-1" />
        )}
        {busy ? "Parsing…" : "Upload xlsx"}
      </Button>
    </>
  );
}
