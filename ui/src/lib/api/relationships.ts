import { gateway } from "@/lib/api-client";

export interface KeyMoment {
  timestamp: string;
  valence: number;
  reaction: string;
  emotion_label: string | null;
}

export interface RelationshipSummary {
  interactions: number;
  active_days: number;
  first_seen: string | null;
  last_seen: string | null;
  days_since_last: number | null;
  sentiment: number;
  band: string;
  gratitude: number;
  praise: number;
  acceptance: number;
  corrections: number;
  retries: number;
  stops: number;
  key_moments: KeyMoment[];
  window_days: number;
}

export const relationshipsApi = {
  mine: (days = 90) =>
    gateway.get<RelationshipSummary>(`/api/me/relationship?days=${days}`),
};
