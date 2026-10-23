import { gateway } from "../api-client";

export type Visibility = "private" | "team" | "public";
export type ArtifactType =
  | "spec"
  | "ticket"
  | "architecture"
  | "notes"
  | "decision"
  | "snippet";

export const ARTIFACT_TYPES: ArtifactType[] = [
  "spec",
  "ticket",
  "architecture",
  "notes",
  "decision",
  "snippet",
];

export const VISIBILITIES: Visibility[] = ["private", "team", "public"];

export interface ArtifactSummary {
  id: string;
  title: string;
  artifact_type: ArtifactType;
  visibility: Visibility;
  tags: string[];
  status: string;
  version: number;
  owner_id: string;
  created_by: string;
  last_modified_by: string;
  created_at: string;
  updated_at: string;
  /** Present on search results only. */
  score?: number;
}

export interface Artifact extends ArtifactSummary {
  content: string;
}

export interface CreatePayload {
  id?: string;
  title: string;
  content: string;
  artifact_type: ArtifactType;
  visibility: Visibility;
  tags: string[];
}

export interface UpdatePayload {
  content?: string;
  visibility?: Visibility;
}

export interface ArtifactFilters {
  type?: ArtifactType;
  tag?: string;
  visibility?: Visibility;
}

function qs(params: Record<string, string | undefined>): string {
  const p = Object.entries(params)
    .filter(([, v]) => v != null && v !== "")
    .map(([k, v]) => `${k}=${encodeURIComponent(v as string)}`);
  return p.length ? `?${p.join("&")}` : "";
}

export const artifactsApi = {
  list: (f: ArtifactFilters = {}) =>
    gateway.get<{ artifacts: ArtifactSummary[] }>(
      `/api/artifacts${qs({ type: f.type, tag: f.tag, visibility: f.visibility })}`,
    ),
  get: (id: string) => gateway.get<Artifact>(`/api/artifacts/${id}`),
  search: (q: string, f: ArtifactFilters = {}) =>
    gateway.get<{ results: ArtifactSummary[] }>(
      `/api/artifacts/search${qs({ q, type: f.type, tag: f.tag })}`,
    ),
  create: (p: CreatePayload) => gateway.post<Artifact>("/api/artifacts", p),
  update: (id: string, p: UpdatePayload) =>
    gateway.put<Artifact>(`/api/artifacts/${id}`, p),
  del: (id: string) => gateway.del<void>(`/api/artifacts/${id}`),
};
