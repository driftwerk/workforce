import type { ExternalSkill } from "@/lib/api/external-skills";
import type { OwnedSkill } from "@/lib/api/owned-skills";

/** Whose a skill is, from the signed-in user's point of view. */
export function ownerNote(
  skill: Pick<OwnedSkill, "owner" | "mine">,
  ownerName?: string,
): string {
  if (skill.owner === null) return "installed for the org";
  if (skill.mine) return "yours";
  return `owned by ${ownerName || "another user"}`;
}

export function toolsNote(count: number): string {
  return `${count} tool${count === 1 ? "" : "s"}`;
}

export function readinessNote(skill: Pick<ExternalSkill, "available">): string {
  return skill.available ? "ready to use" : "not ready";
}

/** Why an external skill is not ready, in the hub's words, which can run over several lines. */
export function readinessWarning(
  skill: Pick<ExternalSkill, "available" | "gating">,
): string | undefined {
  return skill.available ? undefined : skill.gating;
}
