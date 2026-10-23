import { useCallback, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Upload, Download } from "lucide-react";
import { workflowsApi } from "@/lib/api/workflows";
import { parseWorkflowToml } from "@/lib/workflows/toml-parser";
import { useSaveWorkflow } from "@/hooks/use-workflows";

interface ImportExportProps {
  onImported?: () => void;
}

export function ImportExport({ onImported }: ImportExportProps) {
  const fileRef = useRef<HTMLInputElement>(null);
  const saveMutation = useSaveWorkflow();

  const handleImport = useCallback(() => {
    fileRef.current?.click();
  }, []);

  const handleFileChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const file = e.target.files?.[0];
      if (!file) return;

      const reader = new FileReader();
      reader.onload = (ev) => {
        const content = ev.target?.result as string;
        if (!content) return;

        const { def, error } = parseWorkflowToml(content);
        if (error || !def) {
          alert(`Invalid TOML: ${error ?? "Unknown error"}`);
          return;
        }

        saveMutation.mutate(
          { id: def.workflow.id, toml: content },
          {
            onSuccess: () => {
              onImported?.();
            },
            onError: (err) => {
              alert(`Import failed: ${err.message}`);
            },
          },
        );
      };
      reader.readAsText(file);

      // Reset input so the same file can be re-selected
      e.target.value = "";
    },
    [saveMutation, onImported],
  );

  const handleExportAll = useCallback(async () => {
    try {
      const data = await workflowsApi.export();
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = "workflows-export.json";
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, []);

  return (
    <div className="flex items-center gap-2">
      <input ref={fileRef} type="file" accept=".toml" className="hidden" onChange={handleFileChange} />
      <Button variant="outline" size="sm" onClick={handleImport} disabled={saveMutation.isPending}>
        <Upload className="h-3.5 w-3.5 mr-1.5" />
        Import
      </Button>
      <Button variant="outline" size="sm" onClick={handleExportAll}>
        <Download className="h-3.5 w-3.5 mr-1.5" />
        Export All
      </Button>
    </div>
  );
}
