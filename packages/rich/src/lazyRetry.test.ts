import { describe, expect, it } from "vitest";

import { retryImport } from "./lazyRetry";

describe("retryImport", () => {
  it("returns the module on first success without retrying", async () => {
    let calls = 0;
    const result = await retryImport(async () => {
      calls++;
      return { default: "ok" };
    });
    expect(result).toEqual({ default: "ok" });
    expect(calls).toBe(1);
  });

  it("recovers from a transient failure by retrying", async () => {
    let calls = 0;
    const result = await retryImport(
      async () => {
        calls++;
        if (calls < 2) throw new Error("ERR_CONNECTION_CLOSED");
        return { default: "chunk" };
      },
      3,
      0,
    );
    expect(result).toEqual({ default: "chunk" });
    expect(calls).toBe(2);
  });

  it("gives up after the attempt budget and throws the last error", async () => {
    let calls = 0;
    await expect(
      retryImport(
        async () => {
          calls++;
          throw new Error(`fail ${calls}`);
        },
        3,
        0,
      ),
    ).rejects.toThrow("fail 3");
    expect(calls).toBe(3);
  });
});
