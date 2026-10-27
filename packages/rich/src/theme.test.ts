// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";

import { refreshRichTheme, useThemeVersion } from "./theme";

afterEach(() => {
  cleanup();
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.removeAttribute("style");
  document.body.removeAttribute("style");
});

describe("useThemeVersion", () => {
  it("changes when the host flips data-theme on <html>", async () => {
    const { result } = renderHook(() => useThemeVersion());
    const before = result.current;
    act(() => document.documentElement.setAttribute("data-theme", "dark"));
    await waitFor(() => expect(result.current).not.toBe(before));
  });

  it("changes when the host sets a token inline", async () => {
    const { result } = renderHook(() => useThemeVersion());
    const before = result.current;
    act(() => document.documentElement.style.setProperty("--rich-series-1", "#000000"));
    await waitFor(() => expect(result.current).not.toBe(before));
  });

  // A scroll lock or an animation writing styles must not redraw every chart.
  it("ignores style writes that touch no token", async () => {
    const { result } = renderHook(() => useThemeVersion());
    const before = result.current;
    act(() => {
      document.body.style.overflow = "hidden";
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(result.current).toBe(before);
  });

  it("changes on an explicit refresh", () => {
    const { result } = renderHook(() => useThemeVersion());
    const before = result.current;
    act(() => refreshRichTheme());
    expect(result.current).toBe(before + 1);
  });
});
