import { describe, expect, it, vi } from "vitest";

vi.stubGlobal("sessionStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

const { ApiError } = await import("./api-client");
const { accountLine, deviceKindLabel, pairErrorMessage } = await import("./pairing");

describe("pairing wording", () => {
  it("names device kinds for people", () => {
    expect(deviceKindLabel("ios_device")).toBe("iPhone");
    expect(deviceKindLabel("desktop_device")).toBe("Desk");
    expect(deviceKindLabel("voice_device")).toBe("Voice device");
    expect(deviceKindLabel(null)).toBe("iPhone");
    expect(deviceKindLabel("watch_device")).toBe("watch_device");
  });

  it("prefers the server's wording, else the status", () => {
    expect(pairErrorMessage(new ApiError(403, JSON.stringify({ error: "No." })))).toBe("No.");
    expect(pairErrorMessage(new ApiError(404, "Not Found"))).toMatch(/unknown or has expired/);
    expect(pairErrorMessage(new ApiError(500, ""))).toMatch(/Something went wrong/);
    expect(pairErrorMessage(null)).toMatch(/Something went wrong/);
  });

  it("writes the account as name and email", () => {
    expect(accountLine({ display_name: "Kings", email: "k@example.com" })).toBe(
      "Kings (k@example.com)",
    );
    expect(accountLine({ display_name: "", email: "k@example.com" })).toBe("k@example.com");
    expect(accountLine({ display_name: "Kings", email: null })).toBe("Kings");
  });
});
