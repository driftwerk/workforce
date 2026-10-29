import { beforeEach, describe, expect, it, vi } from "vitest";

const store = new Map<string, string>();
vi.stubGlobal("sessionStorage", {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
});

const { RETURN_TO_KEY, safeReturnTo, saveReturnTo, takeReturnTo } = await import("./return-to");

describe("return-to after sign-in", () => {
  beforeEach(() => store.clear());

  it("parks a relative page and restores it once", () => {
    saveReturnTo("/pair?code=ABCD2345EFGH");
    expect(takeReturnTo()).toBe("/pair?code=ABCD2345EFGH");
    expect(takeReturnTo()).toBeNull();
  });

  it.each([
    "https://evil.example/pair",
    "//evil.example/pair",
    "/\\evil.example",
    "/\t/evil.example",
    "javascript:alert(1)",
    "pair?code=x",
    "",
    "/",
  ])("refuses %j", (path) => {
    expect(safeReturnTo(path)).toBeNull();
    saveReturnTo(path);
    expect(store.has(RETURN_TO_KEY)).toBe(false);
  });

  it("drops a parked value tampered into something unsafe", () => {
    store.set(RETURN_TO_KEY, "//evil.example");
    expect(takeReturnTo()).toBeNull();
    expect(store.has(RETURN_TO_KEY)).toBe(false);
  });
});
