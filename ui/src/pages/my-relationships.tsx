import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { StatCard } from "@/components/stats/stat-card";
import { PageSkeleton } from "@/components/loading-skeleton";
import { ApiError } from "@/lib/api-client";
import { useMyRelationship } from "@/hooks/use-relationships";
import type { KeyMoment, RelationshipSummary } from "@/lib/api/relationships";

interface BandInfo {
  label: string;
  tone: string;
  blurb: string;
}

const DEFAULT_BAND: BandInfo = {
  label: "Neutral",
  tone: "text-muted-foreground",
  blurb: "A steady, matter-of-fact working rhythm.",
};

const BANDS: Record<string, BandInfo> = {
  warm: {
    label: "Warm",
    tone: "text-emerald-500",
    blurb: "Recent conversations have been positive and appreciative.",
  },
  positive: {
    label: "Positive",
    tone: "text-green-500",
    blurb: "Things have been going well lately.",
  },
  neutral: DEFAULT_BAND,
  strained: {
    label: "Strained",
    tone: "text-amber-500",
    blurb: "Recent turns leaned frustrated or corrective.",
  },
  quiet: {
    label: "Quiet",
    tone: "text-muted-foreground",
    blurb: "It has been a while since you last spoke.",
  },
};

function recencyText(days: number | null): string {
  if (days === null) return "—";
  if (days <= 0) return "today";
  if (days === 1) return "yesterday";
  return `${days} days ago`;
}

function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

function Hero({ summary }: { summary: RelationshipSummary }) {
  const band = BANDS[summary.band] ?? DEFAULT_BAND;
  return (
    <Card>
      <CardContent className="pt-6">
        <div className="text-xs text-muted-foreground">
          Overall, over the last {summary.window_days} days
        </div>
        <div className={`mt-1 text-3xl font-semibold ${band.tone}`}>{band.label}</div>
        <p className="mt-2 text-sm text-muted-foreground">{band.blurb}</p>
      </CardContent>
    </Card>
  );
}

function ReactionMix({ summary }: { summary: RelationshipSummary }) {
  const data = [
    { name: "Gratitude", value: summary.gratitude },
    { name: "Praise", value: summary.praise },
    { name: "Acceptance", value: summary.acceptance },
    { name: "Corrections", value: summary.corrections },
    { name: "Retries", value: summary.retries },
    { name: "Stops", value: summary.stops },
  ];
  const total = data.reduce((sum, d) => sum + d.value, 0);
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-sm">How you've reacted</CardTitle>
      </CardHeader>
      <CardContent>
        {total === 0 ? (
          <p className="py-8 text-center text-sm text-muted-foreground">
            No strong reactions recorded yet.
          </p>
        ) : (
        <ResponsiveContainer width="100%" height={220}>
          <BarChart
            data={data}
            layout="vertical"
            margin={{ top: 4, right: 12, left: 8, bottom: 4 }}
          >
            <CartesianGrid strokeDasharray="3 3" className="stroke-border" horizontal={false} />
            <XAxis type="number" tick={{ fontSize: 11 }} allowDecimals={false} />
            <YAxis type="category" dataKey="name" tick={{ fontSize: 11 }} width={84} />
            <Tooltip
              contentStyle={{
                backgroundColor: "var(--color-card)",
                border: "1px solid var(--color-border)",
                borderRadius: "0.5rem",
                fontSize: "0.75rem",
              }}
            />
            <Bar dataKey="value" name="Turns" fill="var(--color-primary)" radius={[0, 4, 4, 0]} />
          </BarChart>
        </ResponsiveContainer>
        )}
      </CardContent>
    </Card>
  );
}

function KeyMoments({ moments }: { moments: KeyMoment[] }) {
  if (moments.length === 0) return null;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-sm">Notable moments</CardTitle>
      </CardHeader>
      <CardContent>
        <ul className="space-y-3">
          {moments.map((m, i) => (
            <li
              key={`${m.timestamp}-${m.reaction}-${i}`}
              className="flex items-center gap-3 text-sm"
            >
              <span
                className={`h-2 w-2 shrink-0 rounded-full ${
                  m.valence >= 0 ? "bg-emerald-500" : "bg-amber-500"
                }`}
              />
              <span className="font-medium capitalize">{m.emotion_label ?? m.reaction}</span>
              <span className="ml-auto text-xs text-muted-foreground">
                {formatDate(m.timestamp)}
              </span>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

export default function MyRelationshipsPage() {
  const { data, isLoading, error } = useMyRelationship();

  if (isLoading) return <PageSkeleton />;

  if (error) {
    const msg = error instanceof Error ? error.message : String(error);
    const identityRequired = error instanceof ApiError && error.status === 403;
    return (
      <div className="p-6 space-y-4">
        <h1 className="text-xl font-bold">Your Relationship</h1>
        {identityRequired ? (
          <Card>
            <CardContent className="py-8 text-center space-y-2">
              <p className="text-sm text-muted-foreground">
                This view requires a personal login.
              </p>
              <p className="text-sm text-muted-foreground">
                Sign in with your email or SSO to see your own relationship with the agent.
              </p>
            </CardContent>
          </Card>
        ) : (
          <div className="text-destructive text-sm">Failed to load: {msg}</div>
        )}
      </div>
    );
  }

  const summary = data;
  const empty = !summary || summary.interactions === 0;

  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-xl font-bold">Your Relationship</h1>
        <p className="text-sm text-muted-foreground">
          How your direct conversations with the agent have felt over time — visible only to you.
        </p>
      </div>

      {empty ? (
        <Card>
          <CardContent className="py-8 text-center text-sm text-muted-foreground">
            No relationship data yet. As you chat in direct messages, this builds up.
          </CardContent>
        </Card>
      ) : (
        <>
          <Hero summary={summary} />
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            <StatCard label="Exchanges" value={summary.interactions.toLocaleString()} />
            <StatCard label="Active days" value={summary.active_days.toLocaleString()} />
            <StatCard label="Last spoke" value={recencyText(summary.days_since_last)} />
            <StatCard
              label="Appreciative vs corrective"
              value={`${summary.gratitude + summary.praise + summary.acceptance} / ${
                summary.corrections + summary.retries + summary.stops
              }`}
            />
          </div>
          <div className="grid gap-4 lg:grid-cols-2">
            <ReactionMix summary={summary} />
            <KeyMoments moments={summary.key_moments} />
          </div>
        </>
      )}
    </div>
  );
}
