import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { TokenRow } from "./remote-mcp-tokens-card";
import type { RemoteMcpToken } from "@/lib/api/my-settings";

const token: RemoteMcpToken = {
  id: "5f6a4c3e-0000-4000-8000-000000000001",
  label: "laptop",
  fingerprint: "3fa9c2d1",
  created_at: "2026-10-01T12:00:00Z",
  expires_at: "2026-12-30T12:00:00Z",
  last_used_at: null,
  expired: false,
};

function render(t: RemoteMcpToken): string {
  return renderToStaticMarkup(
    createElement(
      "table",
      null,
      createElement(
        "tbody",
        null,
        createElement(TokenRow, { token: t, onRevoke: () => {}, revoking: false }),
      ),
    ),
  );
}

describe("remote MCP key row", () => {
  it("shows the key's name and fingerprint in the muted style of its neighbours", () => {
    const html = render(token);
    expect(html).toContain("laptop");
    expect(html).toContain("Key 3fa9c2d1");
    expect(html).toContain('class="text-xs text-muted-foreground"');
  });

  it("never renders a key prefix", () => {
    expect(render(token)).not.toContain("wfr_");
  });
});
