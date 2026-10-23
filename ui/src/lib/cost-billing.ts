import type { Billing, BillingSplit, CostDetailRecord } from "@/lib/api/cost";
import type { CostBarRow } from "@/components/stats/cost-bar-chart";
import { formatCostScaled } from "@/lib/utils";

export const ESTIMATE_TITLE = "Estimated list-price value; not billed";

export const UNRECORDED_ORIGIN = "unrecorded (before 2026-09-29)";

export function billingLabel(row: Pick<CostDetailRecord, "billing" | "plan">): string {
  return row.billing === "subscription" ? `Subscription · ${row.plan}` : "Metered";
}

export function billedCost(cost: number, billing: Billing): string {
  return billing === "subscription" ? `~${formatCostScaled(cost)}` : formatCostScaled(cost);
}

export function originLabel(origin: string | null): string {
  return origin === null ? UNRECORDED_ORIGIN : origin.replaceAll("_", " ");
}

export const BILLING_SERIES = [
  { key: "metered", label: "Metered", color: "hsl(221, 83%, 53%)" },
  { key: "subscription", label: "Subscription (est.)", color: "hsl(160, 60%, 45%)" },
] as const;

export function billingBarRow(label: string, split: BillingSplit): CostBarRow {
  return {
    label,
    cost: split.metered_cost_usd + split.subscription_cost_usd,
    parts: { metered: split.metered_cost_usd, subscription: split.subscription_cost_usd },
  };
}
