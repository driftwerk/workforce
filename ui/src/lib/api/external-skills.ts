import type { OwnedSkill } from "./owned-skills";

export interface ExternalSkill extends OwnedSkill {
  author: string;
  available: boolean;
  gating: string;
}
