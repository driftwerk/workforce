import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Play } from "lucide-react";
import type { WorkflowDef } from "@/lib/api/workflows";

const TEMPLATE_IDS = new Set([
  "dev-pipeline",
  "feature-brainstorm",
  "incident-response",
  "bug-fix",
  "batch-dev",
  "codebase-learn",
  "feature-impl",
]);

interface TemplateGalleryProps {
  workflows: WorkflowDef[];
  onUseTemplate: (wf: WorkflowDef) => void;
}

export function TemplateGallery({ workflows, onUseTemplate }: TemplateGalleryProps) {
  const templates = workflows.filter((w) => TEMPLATE_IDS.has(w.id));

  if (templates.length === 0) return null;

  return (
    <div className="space-y-3">
      <div>
        <h2 className="text-lg font-semibold">Templates</h2>
        <p className="text-sm text-muted-foreground">Pre-built workflow templates to get started quickly.</p>
      </div>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
        {templates.map((wf) => (
          <Card key={wf.id} className="flex flex-col">
            <CardHeader className="pb-2 flex-1">
              <CardTitle className="text-sm">{wf.name}</CardTitle>
              <CardDescription className="text-xs line-clamp-2">{wf.description || "No description"}</CardDescription>
            </CardHeader>
            <CardContent className="flex items-center justify-between">
              <Badge variant="secondary" className="text-[10px]">{wf.steps} steps</Badge>
              <Button variant="outline" size="sm" className="h-7 text-xs" onClick={() => onUseTemplate(wf)}>
                <Play className="h-3 w-3 mr-1" />
                Use template
              </Button>
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}
