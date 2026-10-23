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
import type { MonitorDef } from "@/lib/api/monitors";
import { MonitorStatusBadge, troubleHint } from "@/components/monitors/monitor-status";

interface MonitorCardProps {
  monitor: MonitorDef;
  onDuplicate: (m: MonitorDef) => void;
  onExport: (m: MonitorDef) => void;
  onShare: (m: MonitorDef) => void;
  onTransfer: (m: MonitorDef) => void;
  onDelete: (m: MonitorDef) => void;
}

/** What the monitor watches, as badges: its surface and channel, or its tool. */
function WatchBadges({ monitor }: { monitor: MonitorDef }) {
  if (monitor.kind === "source") {
    return (
      <>
        <Badge variant="outline">source</Badge>
        <Badge variant="outline" className="font-mono">{monitor.source}</Badge>
        <Badge variant="outline">{monitor.filter}</Badge>
      </>
    );
  }
  return (
    <>
      <Badge variant="outline">{monitor.surface}</Badge>
      <Badge variant="outline">{monitor.channel}</Badge>
    </>
  );
}

export function MonitorCard({ monitor, onDuplicate, onExport, onShare, onTransfer, onDelete }: MonitorCardProps) {
  const canTransfer = useTransferStanding();
  const navigate = useNavigate();
  const to = `/monitors/${monitor.id}${monitor.owner ? `?owner=${encodeURIComponent(monitor.owner)}` : ""}`;
  const hint = troubleHint(monitor);

  return (
    <Card
      className="cursor-pointer hover:shadow-md transition-shadow group"
      onClick={() => navigate(to)}
    >
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <div className="min-w-0 flex-1">
            <CardTitle className="text-base truncate">{monitor.name}</CardTitle>
            <CardDescription className="line-clamp-2 mt-1">
              {monitor.description || "No description"}
            </CardDescription>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild onClick={(e) => e.stopPropagation()}>
              <Button
                variant="ghost"
                size="icon"
                className="h-8 w-8 opacity-0 group-hover:opacity-100 transition-opacity"
              >
                <MoreHorizontal className="h-4 w-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
              <DropdownMenuItem onClick={() => navigate(to)}>
                <Pencil className="h-3.5 w-3.5 mr-2" />Edit
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onDuplicate(monitor)}>
                <Copy className="h-3.5 w-3.5 mr-2" />Duplicate
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => onExport(monitor)}>
                <Download className="h-3.5 w-3.5 mr-2" />Export
              </DropdownMenuItem>
              {monitor.owner && (
                <DropdownMenuItem onClick={() => onShare(monitor)}>
                  <Share2 className="h-3.5 w-3.5 mr-2" />Share
                </DropdownMenuItem>
              )}
              {canTransfer(monitor.owner) && (
                <DropdownMenuItem onClick={() => onTransfer(monitor)}>
                  <UserPlus className="h-3.5 w-3.5 mr-2" />Transfer
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-destructive focus:text-destructive"
                onClick={() => onDelete(monitor)}
              >
                <Trash2 className="h-3.5 w-3.5 mr-2" />Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-center gap-2 flex-wrap">
          <MonitorStatusBadge status={monitor.status} />
          <WatchBadges monitor={monitor} />
          <Badge variant="secondary">{monitor.action}</Badge>
          {monitor.agent && <Badge variant="outline">@{monitor.agent}</Badge>}
          <ReachBadge visibility={monitor.visibility} />
        </div>
        {hint && <p className="text-xs text-warning line-clamp-2" title={hint}>{hint}</p>}
      </CardContent>
    </Card>
  );
}
