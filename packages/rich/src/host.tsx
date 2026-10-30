// The one seam between the renderers and the app embedding them. Every member
// is optional: a host that cannot page a staged table, fetch a staged file or
// take a form submission leaves it out and the block degrades — a plain notice,
// an unavailable frame, a read-only form — instead of failing.

import { createContext, useContext, useMemo, useRef, type ReactNode, type RefObject } from "react";

import type { SubmitValue, TablePage } from "./spec";
import type { TableParams } from "./tableParams";

export interface RichHost {
  /** One filtered/sorted page of a staged table; also backs a form field's
   *  `options_ref` typeahead. Reject with {@link TableExpired} once the artifact
   *  is gone, so the block says so instead of offering a retry. */
  fetchTablePage?(artifactId: string, params: TableParams): Promise<TablePage>;
  /** A staged file's bytes, for a `wf-image`. The id is a request, never a
   *  grant: the host must resolve it through its own owner-scoped route. */
  fetchFile?(artifactId: string): Promise<Blob>;
  /** Deliver a completed form to the running conversation: `summary` becomes
   *  the user turn, `values` the exact machine arguments. Resolve `true` once the
   *  turn is on its way, `false` if it was dropped before running — the form only
   *  stays locked when it really was sent. */
  submitForm?(summary: string, values: Record<string, SubmitValue>): Promise<boolean>;
  /** Open a link from rendered markdown. Absent, links open in a new tab. */
  openUrl?(url: string): void;
  /** Link schemes kept besides the web's (http, https, mailto), opened through
   *  `openUrl`. Every other scheme is stripped from agent markdown. */
  linkSchemes?: readonly string[];
  /** The form was answered already, here or before: it shows locked and says so. */
  answered?: boolean;
  /** Save a file the block generated (CSV, PNG, Excalidraw scene). Absent, the
   *  browser's anchor download is used. */
  saveFile?(blob: Blob, filename: string): void;
}

/** The staged artifact is gone (TTL-swept); the block shows an expiry note. */
export class TableExpired extends Error {}

interface HostHandle {
  /** The latest host, read at call time: a host object rebuilt on every render
   *  must not refetch a table or restart a typeahead. */
  latest: RefObject<RichHost>;
  /** Which members are present — the only part of the host effects key on. */
  tables: boolean;
  files: boolean;
  forms: boolean;
  answered: boolean;
  /** `linkSchemes`, joined, so a host rebuilt with the same list keeps its renderers. */
  schemes: string;
}

const NO_HOST: HostHandle = {
  latest: { current: {} },
  tables: false,
  files: false,
  forms: false,
  answered: false,
  schemes: "",
};

const HostContext = createContext<HostHandle>(NO_HOST);

export function RichHostProvider({ host, children }: { host: RichHost; children: ReactNode }) {
  const latest = useRef(host);
  latest.current = host;
  const tables = !!host.fetchTablePage;
  const files = !!host.fetchFile;
  const forms = !!host.submitForm;
  const answered = !!host.answered;
  const schemes = (host.linkSchemes ?? []).join(",");
  const handle = useMemo(
    () => ({ latest, tables, files, forms, answered, schemes }),
    [tables, files, forms, answered, schemes],
  );
  return <HostContext.Provider value={handle}>{children}</HostContext.Provider>;
}

export const useHost = (): HostHandle => useContext(HostContext);

/** Hand a blob to the browser as a download. */
export function anchorDownload(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.click();
  // Revoke after the click's synchronous save-initiation, not before, or the
  // browser can cancel the in-flight download.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** The host's file saver, falling back to the anchor download. */
export function useSaveFile(): (blob: Blob, filename: string) => void {
  const { latest } = useHost();
  return (blob, filename) => (latest.current.saveFile ?? anchorDownload)(blob, filename);
}
