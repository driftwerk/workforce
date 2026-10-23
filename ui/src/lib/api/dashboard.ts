import { admin } from "../api-client";

export interface DashboardView {
  agents: { total: number; active: number; idle: number };
  teams: { total: number; total_members: number };
  tasks: { total: number; in_progress: number; completed: number; failed: number };
  nodes: { total: number; online: number; offline: number };
  cost: { today_usd: number; month_usd: number; top_agent: string | null; top_model: string | null };
}

export const dashboardApi = {
  get: () => admin.get<DashboardView>("/api/dashboard"),
  health: () => admin.get<{ status: string; version: string }>("/api/health"),
};
