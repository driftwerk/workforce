// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import WfTable, { columnStyle, displayCell } from "./WfTable";
import { validateTable, type TablePage, type TableSpec } from "./spec";
import { RichHostProvider, TableExpired, type RichHost } from "./host";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function validSpec(raw: unknown): TableSpec {
  const v = validateTable(raw);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
}

const cellTexts = () =>
  Array.from(document.querySelectorAll("tbody td")).map((td) => td.textContent);

describe("displayCell / columnStyle", () => {
  it("formats numeric columns and leaves the rest as raw text", () => {
    expect(displayCell(523797.33, "number", { format: "number" })).toBe("523,797.33");
    expect(displayCell("523797.33", "number", { format: "number" })).toBe("523,797.33");
    expect(displayCell(1024773, "number", { format: "id" })).toBe("1024773");
    expect(displayCell("2026-07-01", "date")).toBe("2026-07-01");
    expect(displayCell(true, "string")).toBe("true");
    expect(displayCell(null, "number")).toBe("");
  });

  it("uses the declared format, else infers from label and values", () => {
    expect(columnStyle({ label: "Fee", format: "currency", unit: "£" }, "number", [[1]], 0)).toEqual({
      format: "currency",
      unit: "£",
    });
    expect(columnStyle({ label: "Lead ID" }, "number", [[1024773]], 0)).toEqual({
      format: "id",
      unit: undefined,
    });
    expect(columnStyle({ label: "Year" }, "string", [["2024"]], 0)).toBeUndefined();
    expect(columnStyle(undefined, "number", [[2024], [2025]], 0)).toEqual({ format: "id", unit: undefined });
  });
});

describe("WfTable inline", () => {
  it("renders separators, currency decimals and untouched identifiers", () => {
    render(
      <WfTable
        spec={validSpec({
          columns: [
            "Lead ID",
            { key: "fee", label: "Total fee", format: "currency", unit: "£" },
            "Days to complete",
            "Year",
          ],
          rows: [
            [1024773, 523797.33, 157.3, 2026],
            [1024774, 9400, 74, 2025],
          ],
        })}
      />,
    );
    expect(cellTexts()).toEqual([
      "1024773", "£523,797.33", "157.3", "2026",
      "1024774", "£9,400.00", "74", "2025",
    ]);
  });

  it("still sorts by value, not by formatted text", () => {
    render(
      <WfTable
        spec={validSpec({
          columns: ["Name", "Amount"],
          rows: [["a", 900], ["b", 1200000], ["c", 15000]],
        })}
      />,
    );
    // Numeric columns sort descending first; a text sort would put "900" above "15,000".
    fireEvent.click(screen.getByRole("button", { name: "Sort by Amount" }));
    expect(cellTexts().filter((_, i) => i % 2 === 1)).toEqual(["1,200,000", "15,000", "900"]);
    fireEvent.click(screen.getByRole("button", { name: "Sort by Amount" }));
    expect(cellTexts().filter((_, i) => i % 2 === 1)).toEqual(["900", "15,000", "1,200,000"]);
  });

  it("finds a row by its formatted or raw number", () => {
    const rows = Array.from({ length: 10 }, (_, i) => [`row ${i}`, (i + 1) * 100000]);
    render(<WfTable spec={validSpec({ columns: ["Name", "Amount"], rows })} />);
    const search = screen.getByLabelText("Search table");
    fireEvent.change(search, { target: { value: "300,000" } });
    expect(cellTexts()).toEqual(["row 2", "300,000"]);
    fireEvent.change(search, { target: { value: "300000" } });
    expect(cellTexts()).toEqual(["row 2", "300,000"]);
  });
});

describe("WfTable inline paging and filters", () => {
  const rows = Array.from({ length: 30 }, (_, i) => [`row ${i}`, i]);
  const names = () => cellTexts().filter((_, i) => i % 2 === 0);

  it("pages client-side and resizes the page", () => {
    render(<WfTable spec={validSpec({ columns: ["Name", "Amount"], rows })} />);
    expect(names()).toHaveLength(10);
    expect(screen.getByText("1 / 3")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Next page" }));
    expect(names()[0]).toBe("row 10");
    fireEvent.change(screen.getByLabelText("Rows per page"), { target: { value: "25" } });
    expect(screen.getByText("30 rows")).toBeTruthy();
    expect(names().length).toBeGreaterThan(0);
  });

  it("filters a numeric column by range and counts what is left", () => {
    render(<WfTable spec={validSpec({ columns: ["Name", "Amount"], rows })} />);
    fireEvent.click(screen.getByRole("button", { name: "Filters" }));
    const [from, to] = screen.getAllByLabelText(/Filter (from|to)/);
    fireEvent.change(from, { target: { value: "5" } });
    fireEvent.change(to, { target: { value: "7" } });
    expect(names()).toEqual(["row 5", "row 6", "row 7"]);
    expect(screen.getByText("3 of 30 rows")).toBeTruthy();
  });
});

describe("WfTable artifact", () => {
  const ARTIFACT = "art_0123456789abcdef0123456789abcdef";
  const fetchTablePage = vi.fn<NonNullable<RichHost["fetchTablePage"]>>();
  const page: TablePage = {
    columns: [
      { key: "lead_id", label: "lead_id", type: "number" },
      { key: "total_fee", label: "total_fee", type: "number" },
    ],
    rows: [[1024773, 523797.33]],
    total: 1,
  };
  const spec = () =>
    validSpec({
      artifact_id: ARTIFACT,
      columns: [{ key: "total_fee", label: "Fee", format: "currency", unit: "£" }],
    });
  const withHost = (host: RichHost) => (
    <RichHostProvider host={host}>
      <WfTable spec={spec()} />
    </RichHostProvider>
  );

  it("applies the column overlay's label and format to the server's columns", async () => {
    fetchTablePage.mockResolvedValue(page);
    render(withHost({ fetchTablePage }));
    await waitFor(() => expect(cellTexts()).toEqual(["1024773", "£523,797.33"]));
    expect(screen.getByRole("button", { name: "Sort by Fee" })).toBeTruthy();
    expect(fetchTablePage).toHaveBeenCalledWith(ARTIFACT, expect.objectContaining({ offset: 0, limit: 25 }));
  });

  it("asks the host for the next sort without a debounce", async () => {
    fetchTablePage.mockResolvedValue(page);
    render(withHost({ fetchTablePage }));
    await waitFor(() => expect(cellTexts()).toHaveLength(2));
    fireEvent.click(screen.getByRole("button", { name: "Sort by Fee" }));
    await waitFor(() =>
      expect(fetchTablePage).toHaveBeenLastCalledWith(
        ARTIFACT,
        expect.objectContaining({ sort: { column: "total_fee", desc: true } }),
      ),
    );
  });

  // Hosts commonly build the host object inline; that must not read as a new
  // data source and refetch the page on every parent render.
  it("does not refetch when the host is re-created with the same members", async () => {
    fetchTablePage.mockResolvedValue(page);
    const { rerender } = render(withHost({ fetchTablePage: (...a) => fetchTablePage(...a) }));
    await waitFor(() => expect(cellTexts()).toHaveLength(2));
    rerender(withHost({ fetchTablePage: (...a) => fetchTablePage(...a) }));
    rerender(withHost({ fetchTablePage: (...a) => fetchTablePage(...a) }));
    expect(fetchTablePage).toHaveBeenCalledTimes(1);
  });

  it("states that the table is gone when the host reports it expired", async () => {
    fetchTablePage.mockRejectedValue(new TableExpired("gone"));
    render(withHost({ fetchTablePage }));
    await screen.findByText(/no longer available/);
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("offers a retry when a page fails for any other reason", async () => {
    fetchTablePage.mockRejectedValueOnce(new Error("HTTP 502")).mockResolvedValue(page);
    render(withHost({ fetchTablePage }));
    fireEvent.click(await screen.findByRole("button", { name: "Retry" }));
    await waitFor(() => expect(cellTexts()).toEqual(["1024773", "£523,797.33"]));
  });

  /// A retry that can never succeed is worse than saying so.
  it("says it cannot be shown when the host does not page tables", () => {
    render(withHost({}));
    expect(screen.getByText(/can.t be shown here/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });
});

describe("WfTable CSV export", () => {
  it("hands the file to the host's saver", async () => {
    const saveFile = vi.fn<NonNullable<RichHost["saveFile"]>>();
    render(
      <RichHostProvider host={{ saveFile }}>
        <WfTable spec={validSpec({ title: "Q3 leads", columns: ["Name", "Amount"], rows: [["a", 1]] })} />
      </RichHostProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: /CSV/ }));
    expect(saveFile).toHaveBeenCalledTimes(1);
    const [blob, filename] = saveFile.mock.calls[0];
    expect(filename).toBe("q3-leads.csv");
    // Decoded with the BOM kept: it is what makes Excel read the file as UTF-8.
    const text = new TextDecoder("utf-8", { ignoreBOM: true }).decode(await blob.arrayBuffer());
    expect(text).toBe("\uFEFFName,Amount\r\na,1\r\n");
  });
});
