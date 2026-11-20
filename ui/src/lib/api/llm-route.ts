import { gateway } from "../api-client";

/** The subscriptions a person may run their work on. */
export type PersonalRouteKind = "claude-code" | "codex" | "devin";

/** What a call on a person's route does when their plan is used up or their
 *  login fails: stop and say so, or run that one call on the hub's models. */
export type RouteFallback = "wait" | "hub";

/** The models a person's route runs by kind of work, in an agent's own
 *  words: `model_preferences.conversation`, `light_model`, `heavy_model`.
 *  Each is a tier (`cheap`, `standard`, `powerful`), one of the route's
 *  `models`, or for light and heavy `chat` (the conversation model). Unset
 *  is the route's default. */
export interface RouteModels {
  model_preferences?: { conversation?: string };
  light_model?: string;
  heavy_model?: string;
}

export interface PersonalRoute extends RouteModels {
  kind: PersonalRouteKind;
  fallback: RouteFallback;
}

/** The kinds of work a route's models are chosen for. */
export type RouteWork = "conversation" | "light" | "heavy";

/** What a route of a kind runs when nothing is chosen, as a picker says it. */
export interface RouteDefaults {
  conversation: string;
  light: string;
  heavy: string;
}

/** The model `route` runs for `work` as set; `undefined` is the default. */
export function chosenModel(route: RouteModels | null | undefined, work: RouteWork): string | undefined {
  if (!route) return undefined;
  const set = work === "conversation" ? route.model_preferences?.conversation : route[`${work}_model`];
  return set?.trim() || undefined;
}

/** `route` with `work` on `model`; `undefined` puts it back on the default. */
export function withModel<R extends object>(route: R, work: RouteWork, model: string | undefined): R & RouteModels {
  const was = route as R & RouteModels;
  const next: R & RouteModels = { ...was, model_preferences: { ...was.model_preferences } };
  if (work === "conversation") {
    if (model) next.model_preferences = { conversation: model };
    else delete next.model_preferences;
  } else if (model) next[`${work}_model`] = model;
  else delete next[`${work}_model`];
  if (next.model_preferences && !next.model_preferences.conversation) delete next.model_preferences;
  return next;
}

/** The models of `route` to keep when it changes to `kind`: all of them on
 *  the same kind, none on another provider's. */
export function keptModels(route: PersonalRoute | null | undefined, kind: PersonalRouteKind): RouteModels {
  if (!route || route.kind !== kind) return {};
  const { model_preferences, light_model, heavy_model } = route;
  return withModel(withModel(withModel({}, "conversation", model_preferences?.conversation), "light", light_model), "heavy", heavy_model);
}

/** A person's setting for one agent: their own route there, or the hub's. */
export type AgentRoute = PersonalRoute | "hub";

/** One model a route can run: what its setting names, and what a person
 *  reads ("Fable 5.2"). */
export interface RouteModel {
  id: string;
  name: string;
  /** The other ids the login lists for this same model (the full id behind
   *  an alias), so a setting naming one reads as this choice. */
  same_as?: string[];
}

/** One kind as the person may pick it where they are; `refused` says why
 *  it can't be picked, and is absent when it can. */
export interface RouteOption {
  kind: PersonalRouteKind;
  title: string;
  refused: string | null;
  /** The ids of `choices`. A hub from before route models were chosen
   *  sends none. */
  models?: string[];
  /** The models a route of this kind can run beside the tiers, by name: on
   *  Claude, the ones the person's own login runs, in its order. A hub from
   *  before names sends only `models`. */
  choices?: RouteModel[];
  /** Whether `choices` are the person's login's own or the hub's. */
  models_from?: "login" | "hub";
  /** Why `choices` aren't the login's own, when they couldn't be read: a
   *  finished sentence, shown as it is. */
  models_note?: string | null;
  defaults?: RouteDefaults;
}

/** The models `option` offers by name; a hub from before names offers its
 *  ids as they are. */
export function routeChoices(option: RouteOption): RouteModel[] {
  return option.choices ?? (option.models ?? []).map((id) => ({ id, name: id }));
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
