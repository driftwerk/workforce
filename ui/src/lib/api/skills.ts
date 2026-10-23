import { gateway } from "../api-client";

export interface SkillCredential {
  vault_path: string;
  description: string;
  required: boolean;
}

export interface WorkspaceCredentialTemplate {
  key: string;
  description: string;
  required: boolean;
}

export interface GlobalSkill {
  name: string;
  dir_name: string;
  description: string;
  type: "builtin" | "external";
  has_config: boolean;
  supports_config?: boolean;
  editable: boolean;
  required_credentials: SkillCredential[];
  setup_hint?: string;
  workspace_vault_prefix?: string;
  workspace_credentials?: WorkspaceCredentialTemplate[];
}

export type CatalogSkillKind = "external" | "wasm";

export interface CatalogSkillView {
  catalog: string;
  name: string;
  kind: CatalogSkillKind;
  description: string;
  version?: string;
  requires_bins?: string[];
  installed: boolean;
  installed_version?: string;
  built_from_source?: boolean;
}

export interface CatalogResult {
  name: string;
  repo: string;
  error?: string;
  skills: CatalogSkillView[];
}

export interface CatalogListing {
  catalogs: CatalogResult[];
}

export interface CatalogInstallOutcome {
  catalog: string;
  skill: string;
  kind: CatalogSkillKind;
  version?: string;
  installed: boolean;
  replaced?: boolean;
  previous_version?: string;
  built_from_source?: boolean;
  active: boolean;
  activation: string;
}

export const skillsApi = {
  list: () => gateway.get<{ skills: GlobalSkill[] }>("/api/skills"),
  get: (name: string) => gateway.get<string>(`/api/skills/${name}`),
  put: (name: string, content: string) =>
    gateway.putText(`/api/skills/${name}`, content),
  delete: (name: string) => gateway.del(`/api/skills/${name}`),
  getConfig: (name: string) =>
    gateway.get<string>(`/api/skills/${name}/config`),
  putConfig: (name: string, content: string) =>
    gateway.putText(`/api/skills/${name}/config`, content),
  catalog: () => gateway.get<CatalogListing>("/api/skills/catalog"),
  installFromCatalog: (catalog: string, name: string) =>
    gateway.post<CatalogInstallOutcome>("/api/skills/catalog/install", {
      catalog,
      name,
    }),
};
