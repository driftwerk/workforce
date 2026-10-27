import { describe, expect, it } from "vitest";

import { tableQueryString } from "./tableParams";

describe("tableQueryString", () => {
  it("encodes offset, limit, sort, search, and JSON filters", () => {
    const qs = tableQueryString({
      offset: 50,
      limit: 25,
      sort: { column: "Due date", desc: true },
      search: "acme & co",
      filters: [{ col: "Amount", op: "gte", value: 100 }],
    });
    const p = new URLSearchParams(qs);
    expect(p.get("offset")).toBe("50");
    expect(p.get("limit")).toBe("25");
    expect(p.get("sort")).toBe("Due date");
    expect(p.get("dir")).toBe("desc");
    expect(p.get("q")).toBe("acme & co");
    expect(JSON.parse(p.get("filters")!)).toEqual([{ col: "Amount", op: "gte", value: 100 }]);
  });

  it("omits absent sort, search, and empty filters", () => {
    const p = new URLSearchParams(tableQueryString({ offset: 0, limit: 10, filters: [] }));
    expect(p.has("sort")).toBe(false);
    expect(p.has("dir")).toBe(false);
    expect(p.has("q")).toBe(false);
    expect(p.has("filters")).toBe(false);
    expect(p.get("offset")).toBe("0");
    expect(p.get("limit")).toBe("10");
  });

  it("marks an ascending sort", () => {
    const p = new URLSearchParams(tableQueryString({ offset: 0, limit: 10, sort: { column: "Name", desc: false } }));
    expect(p.get("dir")).toBe("asc");
  });
});
