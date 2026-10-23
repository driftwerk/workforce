import { gateway } from "../api-client";

export const pairingApi = {
  listPending: () => gateway.get<{ channel: unknown[]; device: unknown[] }>("/api/pair/pending"),
  approve: (code: string) => gateway.post<Record<string, unknown>>(`/api/pair/approve/${code}`),
  reject: (code: string) => gateway.post<Record<string, unknown>>(`/api/pair/reject/${code}`),
  listDevices: () => gateway.get<{ devices: unknown[] }>("/api/pair/devices"),
  revokeDevice: (id: string) => gateway.post<{ revoked: boolean }>(`/api/pair/devices/${id}/revoke`),
};
