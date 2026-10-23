import type { OwnedSkill } from "./owned-skills";

export interface WasmSkill extends OwnedSkill {
  state: string;
  source: string;
  installed_at: string;
  tools: string[];
  capabilities: string[];
  credentials: string[];
}
