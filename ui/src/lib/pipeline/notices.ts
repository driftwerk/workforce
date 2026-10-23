/**
 * Which of a pipeline's notices a viewer sees. A dismissal is the viewer's
 * own and names the notice as it was raised, so raising it again shows it
 * again.
 */

import type { PipelineNotice } from "@/lib/api/pipelines";

export function dismissedKey(scope: string, id: string): string {
  return `pipeline-notices-dismissed:${scope}/${id}`;
}

export function noticeToken(notice: PipelineNotice): string {
  return `${notice.key}@${notice.raised_at}`;
}

export function shownNotices(notices: PipelineNotice[], dismissed: readonly string[]): PipelineNotice[] {
  return notices.filter((notice) => !dismissed.includes(noticeToken(notice)));
}

/** The dismissals to keep once `notice` is dismissed: those of notices no longer raised fall away. */
export function afterDismissing(notices: PipelineNotice[], dismissed: readonly string[], notice: PipelineNotice): string[] {
  const raised = new Set(notices.map(noticeToken));
  return [...new Set([...dismissed.filter((token) => raised.has(token)), noticeToken(notice)])];
}

/** Stored dismissals, whatever the viewer's storage holds. */
export function parseDismissed(raw: string | null): string[] {
  try {
    const value: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(value) ? value.filter((token): token is string => typeof token === "string") : [];
  } catch {
    return [];
  }
}

/** Where a path is resolved, so its origin can be checked without knowing the hub's. */
const SITE = "http://hub.invalid";

function parsed(link: string): URL | null {
  try {
    return new URL(link, SITE);
  } catch {
    return null;
  }
}

/**
 * A link a notice may render, as the browser reads it: an absolute http(s)
 * URL, or a path on this site. A path the browser would take to another host
 * (`//host`, `/\host`, a tab or newline between the slashes) is refused.
 */
export function safeLink(link: string | null): string | null {
  const url = link ? parsed(link) : null;
  if (!link || !url) return null;
  if (link.startsWith("/")) return url.origin === SITE ? `${url.pathname}${url.search}${url.hash}` : null;
  return /^https?:\/\//.test(link) ? url.href : null;
}
