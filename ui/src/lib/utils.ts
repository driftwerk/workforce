import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";
export function cn(...inputs: ClassValue[]) { return twMerge(clsx(inputs)); }

/** Format a duration in milliseconds to a human-readable string. */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || ms <= 0) return "—";
  const totalSec = Math.floor(ms / 1000);
  if (totalSec < 60) return `${totalSec}s`;
  const min = Math.floor(totalSec / 60);
  const sec = totalSec % 60;
  if (min < 60) return sec > 0 ? `${min}m ${sec}s` : `${min}m`;
  const hr = Math.floor(min / 60);
  const remainMin = min % 60;
  if (hr < 24) return remainMin > 0 ? `${hr}h ${remainMin}m` : `${hr}h`;
  const days = Math.floor(hr / 24);
  const remainHr = hr % 24;
  return remainHr > 0 ? `${days}d ${remainHr}h` : `${days}d`;
}

/** Format a cost value to USD string. */
export function formatCost(cost: number | null | undefined): string {
  if (cost == null) return "—";
  return `$${cost.toFixed(4)}`;
}

/** Format a USD cost with precision that scales to the magnitude — 6 decimals
 *  for sub-millicent calls, 4 for sub-cent, 2 for normal amounts. Suitable
 *  for detail tables that mix tiny vision/transcription rows with larger
 *  agent runs. */
export function formatCostScaled(cost: number | null | undefined): string {
  if (cost == null) return "—";
  if (cost === 0) return "$0.00";
  if (cost < 0.001) return `$${cost.toFixed(6)}`;
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(2)}`;
}

/** Format a token count with k/M suffixes for readability in dense tables. */
export function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Format a timestamp to relative time, as seen from `now`. */
export function formatRelativeTime(ts: string | null | undefined, now: Date = new Date()): string {
  if (!ts) return "—";
  try {
    const diff = now.getTime() - new Date(ts).getTime();
    const secs = Math.floor(diff / 1000);
    if (secs < 60) return "just now";
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs}h ago`;
    const days = Math.floor(hrs / 24);
    return `${days}d ago`;
  } catch {
    return ts;
  }
}

/** Format a timestamp as an absolute local date and time. */
export function formatDateTime(ts: string | null | undefined): string {
  if (!ts) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return ts;
  return date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "medium" });
}
