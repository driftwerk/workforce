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
import type { PlaybookDef } from "@/lib/api/playbooks";

interface PlaybookCardProps {
  playbook: PlaybookDef;
  onDuplicate: (pb: PlaybookDef) => void;
  onExport: (pb: PlaybookDef) => void;
  onShare: (pb: PlaybookDef) => void;
  onTransfer: (pb: PlaybookDef) => void;
  onDelete: (pb: PlaybookDef) => void;
}

export function PlaybookCard({ playbook, onDuplicate, onExport, onShare, onTransfer, onDelete }: PlaybookCardProps) {
  const canTransfer = useTransferStanding();
  const navigate = useNavigate();
  // Carry the owner so the editor loads this member's definition, not a
  // same-id global one.
  const to = `/playbooks/${playbook.id}${playbook.owner ? `?owner=${encodeURIComponent(playbook.owner)}` : ""}`;

  return (
    <Card
      className="cursor-pointer hover:shadow-md transition-shadow group"
      onClick={() => navigate(to)}
    >
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <div className="min-w-0 flex-1">
            <CardTitle className="text-base truncate">{playbook.name}</CardTitle>
            <CardDescription className="line-clamp-2 mt-1">{playbook.description || "No description"}</CardDescription>
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
              <DropdownMenuItem onClick={() => onDuplicate(playbook)}>
                <Copy className="h-3.5 w-3.5 mr-2" />Duplicate
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onExport(playbook)}>
                <Download className="h-3.5 w-3.5 mr-2" />Export
              </DropdownMenuItem>
              {playbook.owner && (
                <DropdownMenuItem onClick={() => onShare(playbook)}>
                  <Share2 className="h-3.5 w-3.5 mr-2" />Share
                </DropdownMenuItem>
              )}
              {canTransfer(playbook.owner) && (
                <DropdownMenuItem onClick={() => onTransfer(playbook)}>
                  <UserPlus className="h-3.5 w-3.5 mr-2" />Transfer
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem className="text-destructive focus:text-destructive" onClick={() => onDelete(playbook)}>
                <Trash2 className="h-3.5 w-3.5 mr-2" />Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2">
          <Badge variant="secondary">{playbook.steps} steps</Badge>
          <Badge variant="outline">{playbook.id}</Badge>
          <ReachBadge visibility={playbook.visibility} />
        </div>
      </CardContent>
    </Card>
  );
}
