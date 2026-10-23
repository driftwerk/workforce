import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn(() => Promise.resolve({}));
const post = vi.fn(() => Promise.resolve({}));
const put = vi.fn(() => Promise.resolve({}));
const del = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { get, post, put, del } }));

import type { LifecycleReport, LifecycleStarted } from "./identities";

const { identitiesApi, isPreview, isStarted } = await import("./identities");

const ALICE = "11111111-1111-1111-1111-111111111111";
const BOB = "22222222-2222-2222-2222-222222222222";

describe("identitiesApi lifecycle", () => {
  beforeEach(() => {
    get.mockClear();
    post.mockClear();
  });

  it("reads holdings and the operations recorded for one identity", () => {
    identitiesApi.holdings(ALICE);
    expect(get).toHaveBeenCalledWith(`/api/identities/${ALICE}/holdings`);
    identitiesApi.operations(ALICE);
    expect(get).toHaveBeenLastCalledWith(
      `/api/identity-lifecycle/operations?identity=${ALICE}`,
    );
  });

  it("reads one operation, and tells a started run from a dry run's report", () => {
    identitiesApi.operation(BOB);
    expect(get).toHaveBeenLastCalledWith(`/api/identity-lifecycle/operations/${BOB}`);
    const report: LifecycleReport = {
      operation: "merge",
      subject: ALICE,
      dry_run: true,
      complete: false,
      categories: [],
    };
    expect(isPreview(report)).toBe(true);
    expect(isPreview({ ...report, dry_run: false })).toBe(false);
    const started: LifecycleStarted = {
      operation: {
        id: BOB,
        subject: ALICE,
        kind: "merge",
        target: null,
        actor: "admin",
        status: "running",
        report: {},
        started_at: "",
        lease_until: "",
        finished_at: null,
      },
    };
    expect(isStarted(started)).toBe(true);
    expect(isPreview(started)).toBe(false);
    expect(isStarted(report)).toBe(false);
  });

  it("sends merges and deletes with their dry-run flag and a delete's plan", () => {
    identitiesApi.merge(ALICE, BOB, true);
    expect(post).toHaveBeenCalledWith(`/api/identities/${ALICE}/merge`, {
      into: BOB,
      dry_run: true,
    });
    identitiesApi.deleteWithPlan(ALICE, {
      recipient: BOB,
      choices: { artifacts: "erase" },
      dry_run: false,
    });
    expect(post).toHaveBeenLastCalledWith(`/api/identities/${ALICE}/delete`, {
      recipient: BOB,
      choices: { artifacts: "erase" },
      dry_run: false,
    });
  });

  it("scans for orphans and hands one over or erases it, as a dry run unless told", () => {
    identitiesApi.orphans();
    expect(get).toHaveBeenCalledWith("/api/identity-lifecycle/orphans");
    identitiesApi.transferOrphan(ALICE, { to: BOB, move_personal: false, dry_run: true });
    expect(post).toHaveBeenCalledWith(`/api/identity-lifecycle/orphans/${ALICE}/transfer`, {
      to: BOB,
      move_personal: false,
      dry_run: true,
    });
    identitiesApi.deleteOrphan(ALICE, { recipient: BOB, dry_run: false });
    expect(post).toHaveBeenLastCalledWith(`/api/identity-lifecycle/orphans/${ALICE}/delete`, {
      recipient: BOB,
      dry_run: false,
    });
  });

  it("transfers one item to a recipient", () => {
    identitiesApi.transfer(ALICE, { kind: "job", agent: "a", id: "j1" }, BOB);
    expect(post).toHaveBeenCalledWith(`/api/identities/${ALICE}/transfer`, {
      item: { kind: "job", agent: "a", id: "j1" },
      to: BOB,
    });
  });
});
