// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import WfForm from "./WfForm";
import { validateForm, type FormSpec, type TablePage } from "./spec";
import { RichHostProvider, TableExpired, type RichHost } from "./host";

const ARTIFACT = "art_0123456789abcdef0123456789abcdef";
const disabled = (el: Element) => (el as HTMLButtonElement).disabled;

function validSpec(raw: unknown): FormSpec {
  const v = validateForm(raw);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
}

const fetchTablePage = vi.fn<NonNullable<RichHost["fetchTablePage"]>>();

function renderForm(spec: FormSpec, host: RichHost = { submitForm: vi.fn().mockResolvedValue(true) }) {
  const utils = render(
    <RichHostProvider host={host}>
      <WfForm spec={spec} />
    </RichHostProvider>,
  );
  return { submitForm: host.submitForm, form: utils.container.querySelector("form")!, ...utils };
}

afterEach(() => {
  cleanup();
  fetchTablePage.mockReset();
});

const OWNERS: TablePage = {
  columns: [
    { key: "id", label: "Id", type: "text" },
    { key: "name", label: "Name", type: "text" },
  ],
  rows: [
    ["u_1", "Ann"],
    ["u_2", "Bob"],
  ],
  total: 2,
};

describe("WfForm", () => {
  it("blocks an invalid submit, then submits the summary + values once fixed", () => {
    const spec = validSpec({
      title: "Lead",
      submit_label: "Create",
      fields: [
        { name: "company", label: "Company", type: "text", required: true },
        { name: "status", label: "Status", type: "select", options: [{ value: "open", label: "Open" }], value: "open" },
      ],
    });
    const { submitForm, form } = renderForm(spec);
    const button = screen.getByRole("button", { name: "Create" });
    expect(disabled(button)).toBe(false);

    fireEvent.submit(form);
    expect(submitForm).not.toHaveBeenCalled();
    expect(screen.getByText("Required")).toBeTruthy();
    expect(disabled(button)).toBe(true);

    fireEvent.change(screen.getByLabelText(/Company/), { target: { value: "Acme" } });
    expect(disabled(button)).toBe(false);

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledTimes(1);
    expect(submitForm).toHaveBeenCalledWith(
      'Submitted "Lead"\n- Company: Acme\n- Status: Open',
      { company: "Acme", status: "open" },
    );
    // Locked after submit.
    expect(screen.getByRole("button", { name: "Submitted" })).toBeTruthy();
  });

  it("unlocks again when the submission is dropped before delivery", async () => {
    const spec = validSpec({
      submit_label: "Create",
      fields: [{ name: "company", label: "Company", type: "text", required: true, value: "Acme" }],
    });
    // A turn interrupted out of the queue before it runs resolves `false`.
    const { submitForm } = renderForm(spec, { submitForm: vi.fn().mockResolvedValue(false) });
    const form = screen.getByRole("button", { name: "Create" }).closest("form")!;

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledTimes(1);
    // Optimistically locked, then reopened once delivery reports it never sent —
    // so the user can resubmit instead of a form falsely reading "Submitted".
    expect(screen.getByRole("button", { name: "Submitted" })).toBeTruthy();
    await screen.findByRole("button", { name: "Create" });
  });

  it("maps options_ref rows to typeahead options and submits the picked value", async () => {
    fetchTablePage.mockResolvedValue(OWNERS);
    const spec = validSpec({
      title: "Assign",
      submit_label: "Save",
      fields: [
        { name: "owner", label: "Owner", type: "select", required: true, options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } },
      ],
    });
    const { submitForm, form } = renderForm(spec, {
      fetchTablePage,
      submitForm: vi.fn().mockResolvedValue(true),
    });

    const combo = screen.getByRole("combobox");
    fireEvent.focus(combo);
    fireEvent.change(combo, { target: { value: "an" } });

    // The debounced fetch resolves and its rows become options.
    const option = await screen.findByRole("button", { name: "Ann" });
    fireEvent.mouseDown(option);

    expect(fetchTablePage).toHaveBeenCalledWith(
      ARTIFACT,
      expect.objectContaining({ offset: 0, search: "an" }),
    );

    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledWith('Submitted "Assign"\n- Owner: Ann', { owner: "u_1" });
  });

  /// A host that cannot deliver a submission must not offer a form that looks
  /// live and then silently does nothing.
  it("renders read-only when the host takes no submissions", () => {
    const spec = validSpec({
      submit_label: "Create",
      fields: [{ name: "company", label: "Company", type: "text", value: "Acme" }],
    });
    const { form } = renderForm(spec, {});
    expect(disabled(screen.getByLabelText(/Company/))).toBe(true);
    expect(disabled(screen.getByRole("button", { name: "Create" }))).toBe(true);
    fireEvent.submit(form);
    expect(screen.getByRole("button", { name: "Create" })).toBeTruthy();
  });

  const refSpec = () =>
    validSpec({
      title: "Assign",
      submit_label: "Save",
      fields: [
        { name: "owner", label: "Owner", type: "select", required: true, options_ref: { artifact_id: ARTIFACT, value: "id", label: "name" } },
      ],
    });

  it("falls back to free text for options_ref when the host cannot page tables", () => {
    const submitForm = vi.fn().mockResolvedValue(true);
    const { form } = renderForm(refSpec(), { submitForm });
    expect(screen.queryByRole("combobox")).toBeNull();
    fireEvent.change(screen.getByLabelText(/Owner/), { target: { value: "u_9" } });
    fireEvent.submit(form);
    expect(submitForm).toHaveBeenCalledWith('Submitted "Assign"\n- Owner: u_9', { owner: "u_9" });
  });

  it("falls back to free text once the host reports the options expired", async () => {
    fetchTablePage.mockRejectedValue(new TableExpired("gone"));
    renderForm(refSpec(), { fetchTablePage, submitForm: vi.fn().mockResolvedValue(true) });
    expect(screen.getByRole("combobox")).toBeTruthy();
    await screen.findByPlaceholderText(/Options unavailable/);
    expect(screen.queryByRole("combobox")).toBeNull();
  });
});
