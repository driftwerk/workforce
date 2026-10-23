import { useNavigate } from "react-router";
import { useTransferStanding } from "@/hooks/use-shares";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { MoreHorizontal, Pencil, Copy, Download, Share2, Trash2, UserPlus } from "lucide-react";
import { ReachBadge } from "@/components/sharing/reach-badge";
import type { WorkflowDef } from "@/lib/api/workflows";

interface WorkflowListProps {
  workflows: WorkflowDef[];
  onDuplicate: (wf: WorkflowDef) => void;
  onExport: (wf: WorkflowDef) => void;
  onShare: (wf: WorkflowDef) => void;
  onTransfer: (wf: WorkflowDef) => void;
  onDelete: (wf: WorkflowDef) => void;
}

export function WorkflowList({ workflows, onDuplicate, onExport, onShare, onTransfer, onDelete }: WorkflowListProps) {
  const canTransfer = useTransferStanding();
  const navigate = useNavigate();

  return (
    <div className="border rounded-lg overflow-hidden">
      <table className="w-full">
        <thead>
          <tr className="border-b bg-muted/50">
            <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">Name</th>
            <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden md:table-cell">Description</th>
            <th className="text-center text-xs font-medium text-muted-foreground px-4 py-3 w-20">Steps</th>
            <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-16">Actions</th>
          </tr>
        </thead>
        <tbody>
          {workflows.map((wf) => (
            <tr
              key={`${wf.owner ?? "sys"}/${wf.id}`}
              className="border-b last:border-b-0 hover:bg-muted/30 cursor-pointer transition-colors"
              onClick={() => navigate(`/workflows/${wf.id}${wf.owner ? `?owner=${encodeURIComponent(wf.owner)}` : ""}`)}
            >
              <td className="px-4 py-3">
                <div className="flex items-center gap-2">
                  <span className="font-medium text-sm">{wf.name}</span>
                  <ReachBadge visibility={wf.visibility} className="text-[9px] h-4 px-1" />
                </div>
                <div className="text-xs text-muted-foreground font-mono">{wf.id}</div>
              </td>
              <td className="px-4 py-3 hidden md:table-cell">
                <span className="text-sm text-muted-foreground line-clamp-1">{wf.description || "—"}</span>
              </td>
              <td className="px-4 py-3 text-center">
                <Badge variant="secondary">{wf.steps}</Badge>
              </td>
              <td className="px-4 py-3 text-right">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild onClick={(e) => e.stopPropagation()}>
                    <Button variant="ghost" size="icon" className="h-8 w-8">
                      <MoreHorizontal className="h-4 w-4" />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
                    <DropdownMenuItem onClick={() => navigate(`/workflows/${wf.id}${wf.owner ? `?owner=${encodeURIComponent(wf.owner)}` : ""}`)}>
                      <Pencil className="h-3.5 w-3.5 mr-2" />Edit
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => onDuplicate(wf)}>
                      <Copy className="h-3.5 w-3.5 mr-2" />Duplicate
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => onExport(wf)}>
                      <Download className="h-3.5 w-3.5 mr-2" />Export
                    </DropdownMenuItem>
                    {wf.owner && (
                      <DropdownMenuItem onClick={() => onShare(wf)}>
                        <Share2 className="h-3.5 w-3.5 mr-2" />Share
                      </DropdownMenuItem>
                    )}
                    {canTransfer(wf.owner) && (
                      <DropdownMenuItem onClick={() => onTransfer(wf)}>
                        <UserPlus className="h-3.5 w-3.5 mr-2" />Transfer
                      </DropdownMenuItem>
                    )}
                    <DropdownMenuSeparator />
                    <DropdownMenuItem className="text-destructive focus:text-destructive" onClick={() => onDelete(wf)}>
                      <Trash2 className="h-3.5 w-3.5 mr-2" />Delete
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
