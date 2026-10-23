import { useNavigate } from "react-router";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { MoreHorizontal, Pencil, Copy, Download, Share2, Trash2, UserPlus } from "lucide-react";
import { ReachBadge } from "@/components/sharing/reach-badge";
import { useTransferStanding } from "@/hooks/use-shares";
import type { WorkflowDef } from "@/lib/api/workflows";
import { useAuthStore } from "@/stores/auth-store";

interface WorkflowCardProps {
  workflow: WorkflowDef;
  onDuplicate: (wf: WorkflowDef) => void;
  onExport: (wf: WorkflowDef) => void;
  onShare: (wf: WorkflowDef) => void;
  onTransfer: (wf: WorkflowDef) => void;
  onDelete: (wf: WorkflowDef) => void;
}

export function WorkflowCard({ workflow, onDuplicate, onExport, onShare, onTransfer, onDelete }: WorkflowCardProps) {
  const myId = useAuthStore((s) => s.identity?.id);
  const canTransfer = useTransferStanding();
  const navigate = useNavigate();

  return (
    <Card
      className="cursor-pointer hover:shadow-md transition-shadow group"
      onClick={() => navigate(`/workflows/${workflow.id}${workflow.owner ? `?owner=${encodeURIComponent(workflow.owner)}` : ""}`)}
    >
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <div className="min-w-0 flex-1">
            <CardTitle className="text-base truncate">{workflow.name}</CardTitle>
            <CardDescription className="line-clamp-2 mt-1">{workflow.description || "No description"}</CardDescription>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild onClick={(e) => e.stopPropagation()}>
              <Button variant="ghost" size="icon" className="h-8 w-8 opacity-0 group-hover:opacity-100 transition-opacity">
                <MoreHorizontal className="h-4 w-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
              <DropdownMenuItem onClick={() => navigate(`/workflows/${workflow.id}${workflow.owner ? `?owner=${encodeURIComponent(workflow.owner)}` : ""}`)}>
                <Pencil className="h-3.5 w-3.5 mr-2" />Edit
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onDuplicate(workflow)}>
                <Copy className="h-3.5 w-3.5 mr-2" />Duplicate
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onExport(workflow)}>
                <Download className="h-3.5 w-3.5 mr-2" />Export
              </DropdownMenuItem>
              {workflow.owner && (
                <DropdownMenuItem onClick={() => onShare(workflow)}>
                  <Share2 className="h-3.5 w-3.5 mr-2" />Share
                </DropdownMenuItem>
              )}
              {canTransfer(workflow.owner) && (
                <DropdownMenuItem onClick={() => onTransfer(workflow)}>
                  <UserPlus className="h-3.5 w-3.5 mr-2" />Transfer
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem className="text-destructive focus:text-destructive" onClick={() => onDelete(workflow)}>
                <Trash2 className="h-3.5 w-3.5 mr-2" />Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2">
          <Badge variant="secondary">{workflow.steps} steps</Badge>
          <Badge variant="outline">{workflow.id}</Badge>
          <ReachBadge visibility={workflow.visibility} />
          {workflow.run_as && (
            <Badge variant="outline" title={`Automated runs execute as ${workflow.run_as}`}>
              {workflow.run_as === myId ? "Runs as you" : `Runs as ${workflow.run_as.slice(0, 8)}`}
            </Badge>
          )}
          {workflow.automated_runs_blocked && (
            <Badge variant="warning" title={workflow.automated_runs_blocked}>
              Automation paused
            </Badge>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
