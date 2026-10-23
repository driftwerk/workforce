import { useNavigate } from "react-router";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { MoreHorizontal, Pencil, Copy, Trash2 } from "lucide-react";
import { visibilityIcon } from "@/components/sharing/visibility-select";
import { definitionLabel, definitionVisibility } from "@/lib/access";
import type { ConsolidationTemplateDef } from "@/lib/api/consolidation-templates";

interface Props {
  template: ConsolidationTemplateDef;
  onDuplicate: (t: ConsolidationTemplateDef) => void;
  onDelete: (t: ConsolidationTemplateDef) => void;
}

export function TemplateCard({ template, onDuplicate, onDelete }: Props) {
  const navigate = useNavigate();
  // Carry the owner so the editor loads this member's definition, not a
  // same-id global one.
  const to = `/consolidation-templates/${template.id}${template.owner ? `?owner=${encodeURIComponent(template.owner)}` : ""}`;
  const reach = definitionVisibility(template.visibility);
  const ReachIcon = visibilityIcon(reach);

  return (
    <Card
      className="cursor-pointer hover:shadow-md transition-shadow group"
      onClick={() => navigate(to)}
    >
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <div className="min-w-0 flex-1">
            <CardTitle className="text-base truncate">{template.name}</CardTitle>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild onClick={(e) => e.stopPropagation()}>
              <Button variant="ghost" size="icon" className="h-8 w-8 opacity-0 group-hover:opacity-100 transition-opacity">
                <MoreHorizontal className="h-4 w-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
              <DropdownMenuItem onClick={() => navigate(to)}>
                <Pencil className="h-3.5 w-3.5 mr-2" />Edit
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onDuplicate(template)}>
                <Copy className="h-3.5 w-3.5 mr-2" />Duplicate
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem className="text-destructive focus:text-destructive" onClick={() => onDelete(template)}>
                <Trash2 className="h-3.5 w-3.5 mr-2" />Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2">
          <Badge variant="outline">{template.id}</Badge>
          <Badge variant={reach === "everyone" ? "secondary" : "outline"} className="gap-1">
            <ReachIcon aria-hidden className="h-3 w-3" /> {definitionLabel(template.visibility)}
          </Badge>
        </div>
      </CardContent>
    </Card>
  );
}
