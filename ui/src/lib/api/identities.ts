import { gateway } from "../api-client";

export interface Identity {
  id: string;
  display_name: string;
  email: string | null;
  role: "system_admin" | "manager" | "member";
  status: "active" | "suspended" | "auto_provisioned";
  provisioning_source: "manual" | "slack_auto" | "ios_auto";
  linked_ids: Record<string, string>;
  created_at: string;
  updated_at?: string;
}

export interface IdentityListParams {
  limit: number;
  offset: number;
  search?: string;
}

export interface IdentityListResponse {
  identities: Identity[];
  total: number;
  limit: number;
  offset: number;
}

/** What a merge or a delete does with one category. */
export type MergeRule = "move" | "union" | "revoke" | "drop" | "keep" | "report";
export type LifecycleChoice = "erase" | "transfer";
export type DeleteRule =
  | { rule: "erase" | "release" | "transfer" | "keep" | "expire" | "report" }
  | { rule: "choice"; default: LifecycleChoice };

export type TransferItem =
  | { kind: "job"; agent: string; id: string }
  | { kind: "workflow" | "playbook" | "monitor" | "consolidation_template"; id: string };

/** Whether a category is a missing identity's personal content or work it handed over. */
export type OrphanRule = "personal" | "hand_over";

/** What one hand-over of a missing identity's holdings does with a category. */
export type OrphanAction = "move" | "keep" | "hand_over" | "erase" | "release" | "report";

export interface CategoryReport {
  category: string;
  rules: { merge: MergeRule; delete: DeleteRule; orphan: OrphanRule };
  choice?: LifecycleChoice;
  orphan?: OrphanAction;
  count: number;
  items?: string[];
  conflicts?: string[];
  blocking?: string[];
  movable?: { label: string; item: TransferItem }[];
  error?: string;
}

export interface LifecycleReport {
  operation: "holdings" | "merge" | "delete" | "transfer" | "orphan_transfer" | "orphan_delete";
  subject: string;
  target?: string;
  dry_run: boolean;
  complete: boolean;
  categories: CategoryReport[];
  /** The category under way, while the operation runs. */
  progress?: { category: string; done: number; total: number };
  /** Why it stopped before every category was visited. */
  error?: string;
}

export interface LifecycleOperation {
  id: string;
  subject: string;
  kind: string;
  target: string | null;
  actor: string;
  status: "running" | "complete" | "incomplete" | "interrupted";
  /** Its progress while it runs (empty until the first category starts), then its report. */
  report: Partial<LifecycleReport>;
  started_at: string;
  lease_until: string;
  finished_at: string | null;
}

/** A real run answers with the operation it started, which runs on in the hub. */
export interface LifecycleStarted {
  operation: LifecycleOperation;
}

/** A dry run's report, or the operation a real run started. */
export type LifecycleOutcome = LifecycleReport | LifecycleStarted;

export function isStarted(outcome: LifecycleOutcome): outcome is LifecycleStarted {
  return typeof outcome.operation === "object";
}

export function isPreview(outcome: LifecycleOutcome | undefined): outcome is LifecycleReport {
  return outcome !== undefined && !isStarted(outcome) && outcome.dry_run;
}

/** Counts per category, by category name. */
export type HeldCounts = Record<string, number>;

export interface Orphan {
  id: string;
  /** `recorded`: a lifecycle operation named it; `identity_field`: a field only identity ids are written to holds it. */
  proof: "recorded" | "identity_field";
  names?: string[];
  held: HeldCounts;
}

export interface UnclaimedName {
  name: string;
  could_be?: string[];
  held: HeldCounts;
}

export interface OrphanScan {
  complete: boolean;
  orphans: Orphan[];
  unproven: UnclaimedName[];
  ambiguous: UnclaimedName[];
  not_orphaned: Record<string, number>;
  failed?: Record<string, string>;
}

export interface OrphanTransferRequest {
  to: string;
  /** Also move its conversations, memories, notes and documents; they stay by default. */
  move_personal?: boolean;
  dry_run?: boolean;
}

export interface DeleteRequest {
  recipient?: string;
  choices?: Record<string, LifecycleChoice>;
  dry_run: boolean;
}

export const identitiesApi = {
  list: (params: IdentityListParams) => {
    const query = new URLSearchParams();
    query.set("limit", String(params.limit));
    query.set("offset", String(params.offset));
    if (params.search) query.set("search", params.search);
    return gateway.get<IdentityListResponse>(`/api/identities?${query}`);
  },
  get: (id: string) => gateway.get<Identity>(`/api/identities/${id}`),
  create: (data: { display_name: string; email?: string; role?: string }) =>
    gateway.post<{ id: string; status: string }>("/api/identities", data),
  update: (
    id: string,
    data: {
      display_name?: string;
      email?: string;
      role?: string;
      status?: string;
      linked_ids?: Record<string, string>;
    },
  ) => gateway.put<{ status: string }>(`/api/identities/${id}`, data),
  elevate: (
    id: string,
    data: { email: string; password?: string; role?: string },
  ) => gateway.post<{ status: string }>(`/api/identities/${id}/elevate`, data),
  setPassword: (id: string, password: string) =>
    gateway.post<{ status: string }>(`/api/identities/${id}/set-password`, {
      password,
    }),
  holdings: (id: string) =>
    gateway.get<LifecycleReport>(`/api/identities/${id}/holdings`),
  merge: (id: string, into: string, dryRun: boolean) =>
    gateway.post<LifecycleOutcome>(`/api/identities/${id}/merge`, {
      into,
      dry_run: dryRun,
    }),
  transfer: (id: string, item: TransferItem, to: string) =>
    gateway.post<LifecycleReport>(`/api/identities/${id}/transfer`, { item, to }),
  deleteWithPlan: (id: string, request: DeleteRequest) =>
    gateway.post<LifecycleOutcome>(`/api/identities/${id}/delete`, request),
  orphans: () => gateway.get<OrphanScan>("/api/identity-lifecycle/orphans"),
  transferOrphan: (id: string, request: OrphanTransferRequest) =>
    gateway.post<LifecycleOutcome>(`/api/identity-lifecycle/orphans/${id}/transfer`, request),
  deleteOrphan: (id: string, request: DeleteRequest) =>
    gateway.post<LifecycleOutcome>(`/api/identity-lifecycle/orphans/${id}/delete`, request),
  operations: (id: string) =>
    gateway.get<{ operations: LifecycleOperation[] }>(
      `/api/identity-lifecycle/operations?identity=${encodeURIComponent(id)}`,
    ),
  operation: (id: string) =>
    gateway.get<LifecycleStarted>(`/api/identity-lifecycle/operations/${id}`),
};
