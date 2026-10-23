import { describe, expect, it } from "vitest";
import { formatDuration, formatRelativeTime } from "./utils";

const HOUR = 3_600_000;

describe("formatDuration", () => {
  it("reads values under a day as before", () => {
    expect(formatDuration(45_000)).toBe("45s");
    expect(formatDuration(12 * 60_000 + 3_000)).toBe("12m 3s");
    expect(formatDuration(4 * HOUR + 12 * 60_000)).toBe("4h 12m");
    expect(formatDuration(23 * HOUR + 59 * 60_000)).toBe("23h 59m");
  });

  it("reads a day or more in days and hours", () => {
    expect(formatDuration(24 * HOUR)).toBe("1d");
    expect(formatDuration(26 * HOUR + 3 * 60_000)).toBe("1d 2h");
    expect(formatDuration(72 * HOUR)).toBe("3d");
    expect(formatDuration(29 * HOUR + 12 * 60_000)).toBe("1d 5h");
  });

  it("shows a dash for nothing", () => {
    expect(formatDuration(null)).toBe("—");
    expect(formatDuration(undefined)).toBe("—");
    expect(formatDuration(0)).toBe("—");
    expect(formatDuration(-5)).toBe("—");
  });
});

describe("formatRelativeTime", () => {
  it("reads against the clock it is given", () => {
    const now = new Date("2026-09-15T04:00:00Z");
    expect(formatRelativeTime("2026-09-15T01:00:00Z", now)).toBe("3h ago");
    expect(formatRelativeTime("2026-09-15T03:59:30Z", now)).toBe("just now");
    expect(formatRelativeTime("2026-09-12T04:00:00Z", now)).toBe("3d ago");
  });
});
