import { Link } from "react-router";
import { Cpu } from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { apiErrorMessage } from "@/lib/api-error";
import { useMyAgentRoute, useMyRoute, useSetMyAgentRoute, useSetMyRoute } from "@/hooks/use-llm-route";
import { keptModels, withModel, type PersonalRoute, type PersonalRouteKind, type RouteFallback, type RouteOption } from "@/lib/api/llm-route";
import {
  HUB_CHOICE,
  kindRows,
  ROUTE_LABELS,
  RouteModelsPicker,
  RoutePicker,
  whereDataGoes,
  type RouteChoice,
} from "@/components/subscriptions/route-picker";

const isKind = (choice: RouteChoice): choice is PersonalRouteKind => choice !== "settings" && choice !== "hub";

/** The hub's option for `kind`, when it offers models to choose from. */
const withModels = (options: RouteOption[], kind: PersonalRouteKind | undefined) =>
  options.find((option) => option.kind === kind && option.models !== undefined);

/** Why nothing here can be picked, from the first refusal, when the hub or
 *  the agent runs everyone on its own route. */
function Unavailable({ reason }: { reason: string | null | undefined }) {
  const text = reason ?? "this hub doesn't run people's work on their own subscriptions";
  return <p className="text-sm text-muted-foreground">{text.charAt(0).toUpperCase() + text.slice(1)}.</p>;
}

/** My Settings › Models: the route for every agent that allows it. */
export function MyModelsCard() {
  const { data, isLoading, error } = useMyRoute();
  const save = useSetMyRoute();

  const value: RouteChoice = data?.route?.kind ?? "hub";
  const fallback: RouteFallback = data?.route?.fallback ?? "wait";

  return (
    <Card id="models" className="scroll-mt-6">
      <CardHeader>
        <div className="flex items-center gap-3">
          <div className="rounded-lg bg-muted p-2">
            <Cpu className="h-5 w-5 text-muted-foreground" />
          </div>
          <div>
            <CardTitle className="text-base">Models</CardTitle>
            <CardDescription>
              What your chats, scheduled jobs, workflows and the background work done for you run on, on every
              agent that allows it
            </CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <Skeleton className="h-40 rounded-md" />
        ) : error || !data ? (
          <p className="text-sm text-destructive">Failed to load your model route: {apiErrorMessage(error)}</p>
        ) : !data.available ? (
          <Unavailable reason={data.options[0]?.refused} />
        ) : (
          <RoutePicker
            name="my-route"
            rows={[HUB_CHOICE, ...kindRows(data.options)]}
            value={value}
            fallback={fallback}
            showFallback={isKind(value)}
            dataLine={whereDataGoes(data.route?.kind ?? null)}
            pending={save.isPending}
            error={save.error}
            onChange={(choice, fallback) =>
              save.mutate(
                isKind(choice) ? { ...keptModels(data.route, choice), kind: choice, fallback } : null,
              )
            }
          />
        )}
        {data?.available && data.route && withModels(data.options, data.route.kind) && (
          <div className="mt-4">
            <RouteModelsPicker
              name="my-route-models"
              option={withModels(data.options, data.route.kind)!}
              route={data.route}
              pending={save.isPending}
              onChange={(work, model) => save.mutate(withModel(data.route as PersonalRoute, work, model))}
            />
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** An agent's page: the person's route on this agent alone, "as in my
 *  settings" first. */
export function AgentRouteCard({ agentId }: { agentId: string }) {
  const { data, isLoading, error } = useMyAgentRoute(agentId);
  const save = useSetMyAgentRoute(agentId);

  if (isLoading) return <Skeleton className="h-40 rounded-lg" />;

  const route = data?.route ?? null;
  const value: RouteChoice = route === null ? "settings" : route === "hub" ? "hub" : route.kind;
  const own = route !== null && route !== "hub" ? route : null;
  const fallback: RouteFallback = own?.fallback ?? data?.every_agent?.fallback ?? "wait";
  const everyAgent = data?.every_agent ? ROUTE_LABELS[data.every_agent.kind] : "Hub's models";

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Your models on this agent</CardTitle>
        <CardDescription>
          What your work on this agent runs on, for you alone. Set every agent at once in{" "}
          <Link to="/my-settings#models" className="underline">
            My Settings
          </Link>
          .
        </CardDescription>
      </CardHeader>
      <CardContent>
        {error || !data ? (
          <p className="text-sm text-destructive">Failed to load your model route: {apiErrorMessage(error)}</p>
        ) : !data.available ? (
          <Unavailable reason={data.options[0]?.refused} />
        ) : (
          <RoutePicker
            name={`agent-route-${agentId}`}
            rows={[
              { value: "settings", label: "As in my settings", detail: `Now: ${everyAgent}`, refused: null },
              HUB_CHOICE,
              ...kindRows(data.options),
            ]}
            value={value}
            fallback={fallback}
            showFallback={isKind(value)}
            dataLine={whereDataGoes(data.effective?.kind ?? null)}
            pending={save.isPending}
            error={save.error}
            onChange={(choice, fallback) =>
              save.mutate(
                choice === "settings"
                  ? null
                  : choice === "hub"
                    ? "hub"
                    : { ...keptModels(own, choice), kind: choice, fallback },
              )
            }
          />
        )}
        {data?.available && own && withModels(data.options, own.kind) && (
          <div className="mt-4">
            <RouteModelsPicker
              name={`agent-route-models-${agentId}`}
              option={withModels(data.options, own.kind)!}
              route={own}
              pending={save.isPending}
              onChange={(work, model) => save.mutate(withModel(own, work, model))}
            />
          </div>
        )}
      </CardContent>
    </Card>
  );
}
