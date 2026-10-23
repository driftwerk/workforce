import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { refreshOnSettle } from "./use-subscriptions";

describe("refreshOnSettle", () => {
  it("a sign-in replaced or cancelled elsewhere refreshes the kept sign-in", () => {
    const stale = (state: Parameters<typeof refreshOnSettle>[1]) => {
      const qc = new QueryClient();
      for (const key of [["device-flow-current"], ["my-subscriptions"]]) qc.setQueryData(key, {});
      refreshOnSettle(qc, state);
      return [["device-flow-current"], ["my-subscriptions"]].map((key) => qc.getQueryState(key)?.isInvalidated);
    };
    expect(stale("cancelled")).toEqual([true, false]);
    expect(stale("expired")).toEqual([true, false]);
    expect(stale("completed")).toEqual([true, true]);
    expect(stale("pending")).toEqual([false, false]);
  });
});
