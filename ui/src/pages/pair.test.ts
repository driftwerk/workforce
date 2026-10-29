import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.stubGlobal("sessionStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

const { ApiError } = await import("@/lib/api-client");
const { PairView } = await import("./pair");
type PairState = import("./pair").PairState;

const device = {
  code: "ABCD2345EFGH",
  device_name: "Desk on studio",
  device_kind: "desktop_device",
  requested_at: "2026-10-29T10:00:00Z",
  expires_at: "2026-10-29T10:15:00Z",
};

function render(state: PairState): string {
  return renderToStaticMarkup(
    createElement(PairView, { state, onApprove: () => {}, onReject: () => {} }),
  );
}

const refusal = (status: number, error?: string) =>
  new ApiError(status, error ? JSON.stringify({ error }) : "");

describe("/pair page", () => {
  it("asks to connect the device, showing only what the device said", () => {
    const html = render({ kind: "ready", device, busy: null, failure: null });
    expect(html).toContain("Connect <strong>Desk on studio</strong> to your account?");
    expect(html).toContain("Desk</dd>");
    expect(html).toContain("ABCD2345EFGH");
    expect(html).toContain("Approve");
    expect(html).toContain("Reject");
    expect(html).not.toContain("@");
  });

  it("names the account the device joined", () => {
    const html = render({
      kind: "approved",
      identity: { id: "x", display_name: "Kings", email: "kings@example.com" },
    });
    expect(html).toContain("Connected to Kings (kings@example.com). You can return to your device.");
  });

  it("says plainly when the code is unknown or expired", () => {
    expect(render({ kind: "error", error: refusal(404) })).toContain(
      "This code is unknown or has expired.",
    );
  });

  it("uses the server's words for an already-used code", () => {
    expect(
      render({ kind: "error", error: refusal(409, "This code has already been approved.") }),
    ).toContain("This code has already been approved.");
    expect(render({ kind: "error", error: refusal(409) })).toContain(
      "This code has already been used.",
    );
  });

  it("explains a refusal to connect without leaving the device", () => {
    const html = render({
      kind: "ready",
      device,
      busy: null,
      failure: refusal(403, "Only an administrator can connect devices in this workspace."),
    });
    expect(html).toContain("Only an administrator can connect devices in this workspace.");
    expect(html).toContain("Desk on studio");
  });

  it("asks to wait when rate limited", () => {
    expect(render({ kind: "error", error: refusal(429) })).toContain("Too many attempts.");
  });

  it("confirms a rejection", () => {
    expect(render({ kind: "rejected" })).toContain("The device was not connected.");
  });

  it("says a link without a code has none", () => {
    expect(render({ kind: "no-code" })).toContain("This link has no pairing code.");
  });
});
