import { useState } from "react";

export interface DateRange {
  from: string; // ISO-8601
  to: string; // ISO-8601
}

const PRESETS: { label: string; days: number }[] = [
  { label: "24h", days: 1 },
  { label: "7d", days: 7 },
  { label: "30d", days: 30 },
  { label: "90d", days: 90 },
];

function rangeForDays(days: number): DateRange {
  const to = new Date();
  const from = new Date(to.getTime() - days * 24 * 3600 * 1000);
  return { from: from.toISOString(), to: to.toISOString() };
}

function isoToDateInput(iso: string): string {
  // <input type="date"> expects YYYY-MM-DD.
  return new Date(iso).toISOString().slice(0, 10);
}

function dateInputToIso(d: string, endOfDay = false): string {
  const dt = new Date(`${d}T${endOfDay ? "23:59:59" : "00:00:00"}Z`);
  return dt.toISOString();
}

/**
 * Return the preset whose day-window matches `value`, or "custom" when the
 * range was set explicitly. We compare in *day units* against `to`, not in
 * milliseconds, because the parent state's `to` advances continuously as
 * the page sits open — a strict ISO comparison would never match.
 */
function activePreset(value: DateRange): string {
  const span = new Date(value.to).getTime() - new Date(value.from).getTime();
  const days = Math.round(span / (24 * 3600 * 1000));
  return PRESETS.find((p) => p.days === days)?.label ?? "custom";
}

interface Props {
  value: DateRange;
  onChange: (next: DateRange) => void;
  className?: string;
}

export function DateRangePicker({ value, onChange, className = "" }: Props) {
  const [custom, setCustom] = useState(false);
  const active = custom ? "custom" : activePreset(value);

  return (
    <div className={`flex items-center gap-1 ${className}`}>
      {PRESETS.map((p) => {
        const isActive = !custom && active === p.label;
        return (
          <button
            key={p.label}
            onClick={() => {
              setCustom(false);
              onChange(rangeForDays(p.days));
            }}
            className={`rounded-md px-2.5 py-1 text-xs font-medium transition-colors ${
              isActive
                ? "bg-primary text-primary-foreground"
                : "text-muted-foreground hover:bg-muted"
            }`}
          >
            {p.label}
          </button>
        );
      })}
      <button
        onClick={() => setCustom((c) => !c)}
        className={`rounded-md px-2.5 py-1 text-xs font-medium transition-colors ${
          custom
            ? "bg-primary text-primary-foreground"
            : "text-muted-foreground hover:bg-muted"
        }`}
      >
        Custom
      </button>
      {custom && (
        <div className="flex items-center gap-1 ml-2 text-xs">
          <input
            type="date"
            value={isoToDateInput(value.from)}
            onChange={(e) =>
              onChange({ ...value, from: dateInputToIso(e.target.value) })
            }
            className="rounded-md border border-border bg-background px-2 py-1"
          />
          <span className="text-muted-foreground">→</span>
          <input
            type="date"
            value={isoToDateInput(value.to)}
            onChange={(e) =>
              onChange({ ...value, to: dateInputToIso(e.target.value, true) })
            }
            className="rounded-md border border-border bg-background px-2 py-1"
          />
        </div>
      )}
    </div>
  );
}

export function defaultRange(): DateRange {
  return rangeForDays(30);
}
