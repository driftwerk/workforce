import type { ReactNode } from "react";

export function InlineCode({ children }: { children: ReactNode }) {
  return <code className="text-xs bg-muted px-1 rounded">{children}</code>;
}
