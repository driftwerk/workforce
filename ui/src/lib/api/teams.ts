import { gateway } from "../api-client";

export interface IdentityTeam {
  id: string;
  name: string;
  creator_id: string;
  member_count: number;
  pending_requests: number;
  created_at: string;
}

export interface IdentityTeamDetail {
  id: string;
  name: string;
  creator_id: string;
  members: string[];
  join_requests: { identity_id: string; requested_at: string }[];
  created_at: string;
  updated_at: string;
}

export const teamsApi = {
  listIdentityTeams: () =>
    gateway.get<{ teams: IdentityTeam[]; count: number }>(
      "/api/identity/teams",
    ),
  getIdentityTeam: (id: string) =>
    gateway.get<IdentityTeamDetail>(`/api/identity/teams/${id}`),
  createIdentityTeam: (data: { name: string; creator_id: string }) =>
    gateway.post<{ id: string }>("/api/identity/teams", data),
  updateIdentityTeam: (id: string, data: { name?: string }) =>
    gateway.put(`/api/identity/teams/${id}`, data),
  deleteIdentityTeam: (id: string) =>
    gateway.del(`/api/identity/teams/${id}`),
  addMember: (teamId: string, identityId: string) =>
    gateway.post(`/api/identity/teams/${teamId}/members`, {
      identity_id: identityId,
    }),
  removeMember: (teamId: string, memberId: string) =>
    gateway.del(`/api/identity/teams/${teamId}/members/${memberId}`),
  requestJoin: (teamId: string, identityId: string) =>
    gateway.post(`/api/identity/teams/${teamId}/join-requests`, {
      identity_id: identityId,
    }),
  approveJoin: (teamId: string, applicantId: string) =>
    gateway.post(
      `/api/identity/teams/${teamId}/join-requests/${applicantId}/approve`,
    ),
  rejectJoin: (teamId: string, applicantId: string) =>
    gateway.post(
      `/api/identity/teams/${teamId}/join-requests/${applicantId}/reject`,
    ),
};
