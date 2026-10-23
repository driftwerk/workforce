import { useState } from "react";
import { useNavigate } from "react-router";
import { AlertCircle } from "lucide-react";
import { useAuthStore } from "@/stores/auth-store";
import { ApiError } from "@/lib/api-client";
import {
  useLessonsMetrics,
  useLessonsRecurrences,
} from "@/hooks/use-lessons";
import { AgentSelect, useAgentOptions } from "@/components/lessons/agent-select";
import { SummaryCards } from "@/components/lessons/summary-cards";
import { PreventionCard } from "@/components/lessons/prevention-card";
import { RecallCard } from "@/components/lessons/recall-card";
import { DailyEventsChart } from "@/components/lessons/daily-events-chart";
import { CorpusGrowthChart } from "@/components/lessons/corpus-growth-chart";
import { ByKindChart } from "@/components/lessons/by-kind-chart";
import { RecurrencesList } from "@/components/lessons/recurrences-list";
import { Leaderboard } from "@/components/lessons/leaderboard";
import { LessonBrowser } from "@/components/lessons/lesson-browser";
import { ActionsPanel } from "@/components/lessons/actions-panel";

const DAY_RANGES = [7, 30, 90] as const;

function isForbidden(error: unknown): boolean {
  return error instanceof ApiError && error.status === 403;
}

function ErrorBanner({ error }: { error: unknown }) {
  const forbidden = isForbidden(error);
  return (
    <div className="flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
      <AlertCircle className="h-4 w-4 shrink-0" />
      <span>
        {forbidden
          ? "You need admin access to view Lessons."
          : `Failed to load lessons: ${
              error instanceof Error ? error.message : "Unknown error"
            }`}
      </span>
    </div>
  );
}

export default function LessonsPage() {
  const navigate = useNavigate();
  const identity = useAuthStore((s) => s.identity);
  // Token-only login (no identity) = static gateway token = admin. Email login
  // checks the role, mirroring the sidebar's admin gate.
  const isAdmin = identity ? identity.role === "system_admin" : true;

  const [agent, setAgent] = useState("all");
  const [days, setDays] = useState<number>(30);
  const agentOptions = useAgentOptions();

  const metricsQ = useLessonsMetrics(agent, days);
  const recurrencesQ = useLessonsRecurrences(agent);

  if (identity && !isAdmin) {
    navigate("/", { replace: true });
    return null;
  }

  const metrics = metricsQ.data;
  const error = metricsQ.error ?? recurrencesQ.error;

  return (
    <div className="p-6 space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold">Lessons Learned</h1>
          <p className="text-muted-foreground">
            What the workforce has learned from its mistakes — and whether it
            stops making them.
          </p>
        </div>
        <div className="flex items-center gap-3">
          <AgentSelect value={agent} onChange={setAgent} options={agentOptions} />
          <div className="flex overflow-hidden rounded-md border">
            {DAY_RANGES.map((d) => (
              <button
                key={d}
                onClick={() => setDays(d)}
                className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                  days === d
                    ? "bg-primary text-primary-foreground"
                    : "bg-background text-muted-foreground hover:bg-muted"
                }`}
              >
                {d}d
              </button>
            ))}
          </div>
        </div>
      </div>

      {error && <ErrorBanner error={error} />}

      <SummaryCards summary={metrics?.summary} isLoading={metricsQ.isLoading} />

      <div className="grid gap-4 lg:grid-cols-2">
        <PreventionCard
          prevention={metrics?.prevention ?? null}
          isLoading={metricsQ.isLoading}
        />
        <RecallCard
          recall={metrics?.recall ?? null}
          isLoading={metricsQ.isLoading}
        />
        <ByKindChart byKind={metrics?.by_kind ?? {}} isLoading={metricsQ.isLoading} />
        <DailyEventsChart
          data={metrics?.timeseries ?? []}
          isLoading={metricsQ.isLoading}
        />
        <CorpusGrowthChart
          data={metrics?.corpus_growth ?? []}
          isLoading={metricsQ.isLoading}
        />
      </div>

      <RecurrencesList
        clusters={recurrencesQ.data?.clusters ?? []}
        isLoading={recurrencesQ.isLoading}
      />

      <Leaderboard
        entries={metrics?.leaderboard ?? []}
        isLoading={metricsQ.isLoading}
      />

      <LessonBrowser agent={agent} />

      <ActionsPanel agent={agent} agents={agentOptions} />
    </div>
  );
}
