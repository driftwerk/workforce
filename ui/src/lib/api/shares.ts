import { gateway } from "../api-client";

export type SharePermission = "run" | "manage";
export type PrincipalType = "user" | "team";

/** One grantee of a definition (matches the backend `DefShare`). */
export interface DefShare {
  principal_type: PrincipalType;
  principal_id: string;
  permission: SharePermission;
}

/** The shareable definition kinds, as their REST path prefix. */
export type ShareKind =
  | "workflows"
  | "playbooks"
  | "monitors"
  | "wasm-skills"
  | "external-skills";

function ownerQuery(owner?: string | null): string {
  return owner ? `?owner=${encodeURIComponent(owner)}` : "";
}

/** What a transfer changed, from either the definition or the skill route. */
export interface TransferResult {
  transferred: string;
  from?: string | null;
  to: string;
  to_display: string;
  /** The consequences the page cannot otherwise show, worded by the hub. */
  notes: string[];
  /** Definitions with HTTP triggers: the addresses a transfer leaves alone. */
  trigger_paths?: string[];
  /** WASM skills: shared-scope credentials still serving the previous owner's secret. */
  shared_credentials?: string[];
}

export const ownerApi = {
  /**
   * Hand the resource to `owner`. `from` names the row being transferred out
   * of, the same `?owner=` a definition is shared by; a skill is unique per
   * registry and carries none.
   *
   * `keepManage` leaves the outgoing owner a `manage` grant, written by the
   * hub inside the transfer itself. It cannot be sent as a share beforehand:
   * until the handover lands they are still the owner, and an owner is never
   * their own grantee.
   */
  set: (
    kind: ShareKind,
    id: string,
    owner: string,
    from: string | null | undefined,
    keepManage: boolean,
  ) =>
    gateway.put<TransferResult>(`/api/${kind}/${id}/owner${ownerQuery(from)}`, {
      owner,
      keep_manage: keepManage,
    }),
};

export const sharesApi = {
  get: (kind: ShareKind, id: string, owner?: string | null) =>
    gateway.get<{ shares: DefShare[] }>(
      `/api/${kind}/${id}/shares${ownerQuery(owner)}`,
    ),
  /** Replace the full grantee set (the dialog's selection is authoritative). */
  set: (kind: ShareKind, id: string, shares: DefShare[], owner?: string | null) =>
    gateway.put<{ shared: string; count: number }>(
      `/api/${kind}/${id}/shares${ownerQuery(owner)}`,
      { shares },
    ),
};
