import { useState } from "react";
import { ExternalLink, X } from "lucide-react";
import { cn } from "@/lib/utils";
import type { PipelineNotice } from "@/lib/api/pipelines";
import { readStored, writeStored } from "@/lib/local-store";
import { afterDismissing, dismissedKey, noticeToken, parseDismissed, safeLink, shownNotices } from "@/lib/pipeline/notices";
import { TONE_FILL, TONE_NAME } from "./tone";

/** What the pipeline wants everyone watching its board to know now, one line each; dismissing one hides it for this viewer only. */
export function Notices({ scope, id, notices }: { scope: string; id: string; notices: PipelineNotice[] }) {
  const key = dismissedKey(scope, id);
  const [dismissed, setDismissed] = useState(() => parseDismissed(readStored(key)));
  const shown = shownNotices(notices, dismissed);
  if (shown.length === 0) return null;

  const dismiss = (notice: PipelineNotice) => {
    const next = afterDismissing(notices, dismissed, notice);
    setDismissed(next);
    writeStored(key, JSON.stringify(next));
  };

  return (
    <ul className="space-y-2" aria-label="Notices">
      {shown.map((notice) => {
        const link = safeLink(notice.link);
        const external = link !== null && !link.startsWith("/");
        return (
          <li
            key={noticeToken(notice)}
            className="relative flex items-start gap-2 overflow-hidden rounded-md border bg-card py-2 pl-4 pr-2 text-sm shadow-sm"
          >
            <span aria-hidden className={cn("absolute inset-y-0 left-0 w-1", TONE_FILL[notice.tone])} />
            <p className="min-w-0 flex-1 break-words">
              <span className="sr-only">{TONE_NAME[notice.tone]}: </span>
              <span className="font-medium">{notice.title}</span>
              {notice.detail && <span className="text-muted-foreground"> · {notice.detail}</span>}
            </p>
            {link && (
              <a
                href={link}
                target={external ? "_blank" : undefined}
                rel={external ? "noreferrer" : undefined}
                aria-label={`Open ${notice.title}`}
                className="inline-flex shrink-0 items-center gap-0.5 rounded text-xs text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
              >
                Open
                {external && <ExternalLink className="h-3 w-3" />}
              </a>
            )}
            <button
              type="button"
              onClick={() => dismiss(notice)}
              aria-label={`Dismiss ${notice.title}`}
              className="shrink-0 rounded p-0.5 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </li>
        );
      })}
    </ul>
  );
}
