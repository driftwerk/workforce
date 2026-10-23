import type { Tone } from "@/lib/pipeline/definition";

/** A tone as a fill, for a stripe or a dot; the same colour in both themes. */
export const TONE_FILL: Record<Tone, string> = {
  active: "bg-primary",
  good: "bg-success",
  warn: "bg-warning",
  healing: "bg-sky-500",
  bad: "bg-red-500",
  neutral: "bg-muted-foreground/60",
};

/** A tone in words, as the board's colour legend reads it, for whoever cannot see the colour. */
export const TONE_NAME: Record<Tone, string> = {
  active: "in progress",
  good: "passed",
  warn: "needs a person",
  healing: "self-healing",
  bad: "halted",
  neutral: "note",
};
