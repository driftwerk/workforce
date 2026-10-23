import { Card, CardContent } from "@/components/ui/card";

const STAT_COLOUR = {
  good: "text-success",
  bad: "text-red-600 dark:text-red-500",
  warn: "text-warning",
  healing: "text-sky-600 dark:text-sky-400",
  muted: "text-muted-foreground",
} as const;

export function Stat({
  label,
  value,
  tone,
  hint,
}: {
  label: string;
  value: number | string;
  tone?: keyof typeof STAT_COLOUR;
  /** A muted line under the value. */
  hint?: string | null;
}) {
  const colour = tone ? STAT_COLOUR[tone] : "";
  return (
    <Card>
      <CardContent className="px-4 py-3">
        <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className={`text-2xl font-semibold tabular-nums ${colour}`}>{value}</div>
        {hint && <div className="mt-0.5 text-xs text-muted-foreground">{hint}</div>}
      </CardContent>
    </Card>
  );
}
