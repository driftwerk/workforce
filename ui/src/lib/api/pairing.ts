import { gateway } from "../api-client";

/** A device waiting for someone to connect it, as the `/pair` page sees it. */
export interface PendingDevice {
  code: string;
  device_name: string;
  device_kind: string;
  requested_at: string;
  expires_at: string;
}

/** The account a device was connected to. */
export interface PairedIdentity {
  id: string;
  display_name: string;
  email: string | null;
}

export interface DeviceApproved {
  approved: true;
  device_id: string;
  device_name: string;
  identity: PairedIdentity;
}

export const pairingApi = {
  listPending: () => gateway.get<{ channel: unknown[]; device: unknown[] }>("/api/pair/pending"),
  approve: (code: string) => gateway.post<Record<string, unknown>>(`/api/pair/approve/${code}`),
  reject: (code: string) => gateway.post<Record<string, unknown>>(`/api/pair/reject/${code}`),
  listDevices: () => gateway.get<{ devices: unknown[] }>("/api/pair/devices"),
  revokeDevice: (id: string) => gateway.post<{ revoked: boolean }>(`/api/pair/devices/${id}/revoke`),
  /** The waiting device behind `code`, for the signed-in person to confirm. */
  devicePending: (code: string) =>
    gateway.get<PendingDevice>(`/api/pair/device/pending/${encodeURIComponent(code)}`),
  /** Connect the device behind `code` to the signed-in person's own account. */
  approveSelf: (code: string) =>
    gateway.post<DeviceApproved>("/api/pair/device/approve-self", { code }),
  /** Turn the device behind `code` away. */
  rejectSelf: (code: string) =>
    gateway.post<{ rejected: true }>("/api/pair/device/reject-self", { code }),
  /** Admin: connect the device behind `code` to the chosen person. */
  approveInto: (code: string, identityId: string) =>
    gateway.post<DeviceApproved>("/api/pair/device/approve", {
      code,
      identity_id: identityId,
    }),
};
