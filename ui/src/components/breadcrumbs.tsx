import { Link, useLocation } from "react-router";
import { ChevronRight, Home } from "lucide-react";

const LABEL_MAP: Record<string, string> = {
  "": "Dashboard",
  chat: "Chat",
  sessions: "Sessions",
  agents: "Agents",
  teams: "Teams",
  workflows: "Workflows",
  scheduler: "Scheduler",
  audit: "Audit",
  pairing: "Pairing",
  vault: "Vault",
  settings: "Settings",
};

export function Breadcrumbs() {
  const location = useLocation();
  const segments = location.pathname.split("/").filter(Boolean);

  if (segments.length === 0) return null;

  return (
    <nav className="flex items-center gap-1 text-sm text-muted-foreground" aria-label="Breadcrumb">
      <Link to="/" className="flex items-center gap-1 hover:text-foreground transition-colors">
        <Home className="h-3.5 w-3.5" />
      </Link>
      {segments.map((segment, i) => {
        const path = "/" + segments.slice(0, i + 1).join("/");
        const label = LABEL_MAP[segment] ?? segment;
        const isLast = i === segments.length - 1;
        return (
          <span key={path} className="flex items-center gap-1">
            <ChevronRight className="h-3.5 w-3.5" />
            {isLast ? (
              <span className="font-medium text-foreground">{label}</span>
            ) : (
              <Link to={path} className="hover:text-foreground transition-colors">{label}</Link>
            )}
          </span>
        );
      })}
    </nav>
  );
}
