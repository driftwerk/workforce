import { gateway } from "../api-client";

/** The subscriptions a person may run their work on. */
export type PersonalRouteKind = "claude-code" | "codex" | "devin";

/** What a call on a person's route does when their plan is used up or their
 *  login fails: stop and say so, or run that one call on the hub's models. */
export type RouteFallback = "wait" | "hub";

export interface PersonalRoute {
  kind: PersonalRouteKind;
  fallback: RouteFallback;
}

/** A person's setting for one agent: their own route there, or the hub's. */
export type AgentRoute = PersonalRoute | "hub";

/** One kind as the person may pick it where they are; `refused` says why
 *  it can't be picked, and is absent when it can. */
export interface RouteOption {
  kind: PersonalRouteKind;
  title: string;
  refused: string | null;
}

export interface MyRouteView {
  available: boolean;
  allow: PersonalRouteKind[];
  route: PersonalRoute | null;
  options: RouteOption[];
}

export interface MyAgentRouteView {
  agent_id: string;
  available: boolean;
  allow: PersonalRouteKind[];
  route: AgentRoute | null;
  every_agent: PersonalRoute | null;
  effective: PersonalRoute | null;
  options: RouteOption[];
}

const agentPath = (agentId: string) => `/api/me/agents/${encodeURIComponent(agentId)}/llm-route`;

export const llmRouteApi = {
  /** GET /api/me/llm-route — the caller's setting for every agent. */
  mine: () => gateway.get<MyRouteView>("/api/me/llm-route"),
  /** PUT /api/me/llm-route — `null` clears it back to the hub's models. */
  setMine: (route: PersonalRoute | null) => gateway.put<MyRouteView>("/api/me/llm-route", { route }),
  /** GET /api/me/agents/{id}/llm-route — the caller's setting for one agent. */
  agent: (agentId: string) => gateway.get<MyAgentRouteView>(agentPath(agentId)),
  /** PUT /api/me/agents/{id}/llm-route — `null` goes back to "as in my settings". */
  setAgent: (agentId: string, route: AgentRoute | null) =>
    gateway.put<MyAgentRouteView>(agentPath(agentId), { route }),
};
