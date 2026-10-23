import { gateway } from "../api-client";
import type { Visibility } from "../access";
import type { ExternalSkill } from "./external-skills";
import type { DefShare } from "./shares";
import type { WasmSkill } from "./wasm-skills";

/** The skill each owned kind's routes return, keyed by their REST path prefix. */
export interface OwnedSkillKinds {
  "wasm-skills": WasmSkill;
  "external-skills": ExternalSkill;
}

export type OwnedSkillKind = keyof OwnedSkillKinds;

/** Who may use a skill beyond its owner and explicit grantees. */
export type SkillVisibility = Visibility;

/** The caller's own standing on a skill. */
export type SkillAccess = "run" | "manage";

export interface OwnedSkill {
  name: string;
  version: string;
  description: string;
  /** `null` for skills installed for the whole org (no per-user owner). */
  owner: string | null;
  /** Whether the signed-in user is the owner. */
  mine: boolean;
  /** Whether they decide who reaches it — the owner (or an admin), never a
   * `manage` grantee, who may edit the skill but not re-share or publish it. */
  can_share: boolean;
  visibility: SkillVisibility;
  shares: DefShare[];
  access: SkillAccess;
}

export const ownedSkillsApi = {
  list: <K extends OwnedSkillKind>(kind: K) =>
    gateway.get<{ skills: OwnedSkillKinds[K][] }>(`/api/${kind}`),
  setVisibility: (kind: OwnedSkillKind, name: string, visibility: SkillVisibility) =>
    gateway.put<{ skill: string; visibility: SkillVisibility }>(
      `/api/${kind}/${encodeURIComponent(name)}/visibility`,
      { visibility },
    ),
  uninstall: (kind: OwnedSkillKind, name: string) =>
    gateway.del(`/api/${kind}/${encodeURIComponent(name)}`),
};
