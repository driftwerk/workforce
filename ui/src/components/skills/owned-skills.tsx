import { useState } from "react";
import {
  Boxes,
  FileText,
  Loader2,
  Share2,
  Trash2,
  UserPlus,
  Users,
  type LucideIcon,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ShareDialog } from "@/components/sharing/share-dialog";
import { TransferDialog } from "@/components/sharing/transfer-dialog";
import {
  VisibilitySelect,
  visibilityIcon,
} from "@/components/sharing/visibility-select";
import {
  useOwnedSkills,
  useRefreshOwnedSkills,
  useSetSkillVisibility,
  useUninstallOwnedSkill,
} from "@/hooks/use-owned-skills";
import { useIdentities } from "@/hooks/use-identities";
import type { OwnedSkillKind, OwnedSkillKinds } from "@/lib/api/owned-skills";
import { apiErrorMessage } from "@/lib/api-error";
import { visibilityChoices, visibilityLabel } from "@/lib/access";
import { ownerNote, readinessNote, readinessWarning, toolsNote } from "@/lib/skills/owned";

/** How the Skills page shows one kind of owned skill. */
interface KindView<K extends OwnedSkillKind> {
  title: string;
  /** The kind as it reads inside a sentence. */
  noun: string;
  icon: LucideIcon;
  summary: (skill: OwnedSkillKinds[K]) => string;
  warning?: (skill: OwnedSkillKinds[K]) => string | undefined;
}

const KIND_VIEWS: { [K in OwnedSkillKind]: KindView<K> } = {
  "external-skills": {
    title: "External skills",
    noun: "external skills",
    icon: FileText,
    summary: readinessNote,
    warning: readinessWarning,
  },
  "wasm-skills": {
    title: "WASM skills",
    noun: "WASM skills",
    icon: Boxes,
    summary: (skill) => toolsNote(skill.tools.length),
  },
};

/**
 * Skills of one kind the signed-in user can use: their own, ones shared with
 * them, and ones for everyone. Owners share them, publish them to everyone and
 * uninstall them here; writing a skill happens in chat with the agent.
 */
export function OwnedSkills<K extends OwnedSkillKind>({ kind }: { kind: K }) {
  const { title, noun, icon: Icon } = KIND_VIEWS[kind];
  const { data, isLoading, error } = useOwnedSkills(kind);
  const skills = data?.skills ?? [];

  if (!isLoading && !error && skills.length === 0) return null;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Icon aria-hidden className="h-4 w-4 text-muted-foreground" />
        <h2 className="text-sm font-semibold">{title}</h2>
      </div>
      {isLoading ? (
        <div className="flex items-center justify-center h-24">
          <Loader2 aria-hidden className="h-5 w-5 animate-spin text-muted-foreground" />
        </div>
      ) : error ? (
        <div className="text-xs text-destructive">
          Failed to load {noun}: {apiErrorMessage(error)}
        </div>
      ) : (
        <ul className="divide-y rounded-lg border bg-card">
          {skills.map((skill) => (
            <OwnedSkillRow key={skill.name} kind={kind} skill={skill} />
          ))}
        </ul>
      )}
    </div>
  );
}

function OwnedSkillRow<K extends OwnedSkillKind>({
  kind,
  skill,
}: {
  kind: K;
  skill: OwnedSkillKinds[K];
}) {
  const { summary, warning } = KIND_VIEWS[kind];
  const identities = useIdentities();
  const setVisibility = useSetSkillVisibility(kind);
  const uninstall = useUninstallOwnedSkill(kind);
  const refresh = useRefreshOwnedSkills(kind);
  const [shareOpen, setShareOpen] = useState(false);
  const [transferOpen, setTransferOpen] = useState(false);
  const [confirmingRemove, setConfirmingRemove] = useState(false);

  const canManage = skill.access === "manage";
  const published = skill.visibility === "everyone";
  const VisibilityIcon = visibilityIcon(skill.visibility);
  const owner = skill.owner
    ? identities.data?.identities.find((u) => u.id === skill.owner)
    : undefined;
  const caution = warning?.(skill);
  const busy = setVisibility.isPending || uninstall.isPending;
  const failure = setVisibility.error ?? uninstall.error;

  return (
    <li className="flex items-start justify-between gap-3 px-4 py-3">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium truncate">{skill.name}</span>
          <span className="text-[10px] text-muted-foreground">v{skill.version}</span>
          <Badge variant={published ? "secondary" : "outline"} className="text-[10px] shrink-0 gap-1">
            <VisibilityIcon aria-hidden className="h-3 w-3" />
            {visibilityLabel(skill.visibility)}
          </Badge>
          {skill.shares.length > 0 && (
            <Badge variant="outline" className="text-[10px] shrink-0 gap-1">
              <Users aria-hidden className="h-3 w-3" />
              <span className="sr-only">Shared with </span>
              {skill.shares.length}
            </Badge>
          )}
        </div>
        {skill.description && (
          <p className="text-xs text-muted-foreground mt-1 line-clamp-2">
            {skill.description}
          </p>
        )}
        <p className="text-[10px] text-muted-foreground mt-1">
          {`${summary(skill)} · ${ownerNote(skill, owner?.display_name || owner?.email || undefined)}`}
        </p>
        {caution && (
          <p className="text-[10px] text-warning mt-1 whitespace-pre-line">{caution}</p>
        )}
        {failure && (
          <p className="text-[10px] text-destructive mt-1">{apiErrorMessage(failure)}</p>
        )}
      </div>
      {canManage && (
        <div className="flex shrink-0 items-center gap-1">
          {skill.can_share && (
            <VisibilitySelect
              value={skill.visibility}
              options={visibilityChoices(skill.owner === null)}
              disabled={busy}
              onChange={(visibility) =>
                setVisibility.mutate({ name: skill.name, visibility })
              }
            />
          )}
          {skill.can_share && skill.owner !== null && (
            <>
              <Button size="sm" variant="outline" onClick={() => setShareOpen(true)}>
                <Share2 aria-hidden className="h-3.5 w-3.5" />
                Share
              </Button>
              {/* `can_share` is ownership or the bypass, never a `manage`
                  grantee — exactly who the route lets transfer. */}
              <Button size="sm" variant="outline" onClick={() => setTransferOpen(true)}>
                <UserPlus aria-hidden className="h-3.5 w-3.5" />
                Transfer
              </Button>
            </>
          )}
          <Button
            size="sm"
            variant={confirmingRemove ? "destructive" : "outline"}
            disabled={busy}
            onClick={() =>
              confirmingRemove
                ? uninstall.mutate(skill.name)
                : setConfirmingRemove(true)
            }
            onBlur={() => setConfirmingRemove(false)}
          >
            {uninstall.isPending ? (
              <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Trash2 aria-hidden className="h-3.5 w-3.5" />
            )}
            {confirmingRemove ? "Confirm uninstall" : "Uninstall"}
          </Button>
        </div>
      )}
      {skill.can_share && skill.owner !== null && (
        <ShareDialog
          kind={kind}
          id={skill.name}
          owner={skill.owner}
          name={skill.name}
          open={shareOpen}
          onOpenChange={(open) => {
            setShareOpen(open);
            if (!open) refresh();
          }}
        />
      )}
      {skill.can_share && skill.owner !== null && (
        <TransferDialog
          kind={kind}
          id={skill.name}
          owner={skill.owner}
          name={skill.name}
          open={transferOpen}
          onOpenChange={(open) => {
            setTransferOpen(open);
            if (!open) refresh();
          }}
        />
      )}
    </li>
  );
}
