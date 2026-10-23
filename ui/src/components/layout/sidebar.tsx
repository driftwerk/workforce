import { useEffect, useState } from "react";
import { NavLink } from "react-router";
import { cn } from "@/lib/utils";
import {
  LayoutDashboard, MessageSquare, Users, UserCog, User, Bot, Network, GitBranch,
  Clock, Shield, ShieldCheck, Key, Settings, FileText, Puzzle, BarChart3, DollarSign, BookOpen,
  BookMarked, Library, Eye, GraduationCap, Layers, HeartHandshake, Gamepad2, ArrowLeftRight,
  PiggyBank, Workflow,
} from "lucide-react";
import { useAuthStore } from "@/stores/auth-store";
import { gateway } from "@/lib/api-client";

interface NavItem {
  label: string;
  path: string;
  icon: typeof LayoutDashboard;
  adminOnly?: boolean;
}

interface NavSection {
  title?: string;
  items: NavItem[];
}

const NAV_SECTIONS: NavSection[] = [
  {
    items: [
      { label: "Dashboard", path: "/", icon: LayoutDashboard },
      { label: "Chat", path: "/chat", icon: MessageSquare },
      { label: "Games", path: "/games", icon: Gamepad2 },
    ],
  },
  {
    title: "Work",
    items: [
      { label: "Sessions", path: "/sessions", icon: FileText },
      { label: "Workflows", path: "/workflows", icon: GitBranch },
      { label: "Pipelines", path: "/pipelines", icon: Workflow },
      { label: "Playbooks", path: "/playbooks", icon: BookMarked },
      { label: "Consolidation", path: "/consolidation-templates", icon: Layers },
      { label: "Monitors", path: "/monitors", icon: Eye },
      { label: "Scheduler", path: "/scheduler", icon: Clock },
    ],
  },
  {
    title: "Workforce",
    items: [
      { label: "Agents", path: "/agents", icon: Bot },
      { label: "Teams", path: "/teams", icon: Users },
      { label: "Skills", path: "/skills", icon: Puzzle },
    ],
  },
  {
    title: "Knowledge",
    items: [
      { label: "Artifacts", path: "/artifacts", icon: Library },
      { label: "Docs", path: "/docs", icon: BookOpen },
    ],
  },
  {
    title: "Insights",
    items: [
      { label: "Stats", path: "/stats", icon: BarChart3 },
      { label: "Costs", path: "/costs", icon: DollarSign },
    ],
  },
  {
    title: "Admin",
    items: [
      { label: "Users", path: "/users", icon: UserCog, adminOnly: true },
      { label: "Budget", path: "/budget", icon: PiggyBank, adminOnly: true },
      { label: "Policy", path: "/policy", icon: ShieldCheck, adminOnly: true },
      { label: "Permissions", path: "/permissions", icon: ShieldCheck, adminOnly: true },
      { label: "Audit", path: "/audit", icon: Shield, adminOnly: true },
      { label: "Lessons", path: "/lessons", icon: GraduationCap, adminOnly: true },
      { label: "Vault", path: "/vault", icon: Key, adminOnly: true },
      { label: "Transfer", path: "/transfer", icon: ArrowLeftRight, adminOnly: true },
      { label: "Settings", path: "/settings", icon: Settings, adminOnly: true },
    ],
  },
];

// Personal items live next to the identity block, not in a topic section.
const BOTTOM_ITEMS: NavItem[] = [
  { label: "My Permissions", path: "/my-permissions", icon: ShieldCheck },
  { label: "Pairing", path: "/pairing", icon: Network },
  { label: "Relationship", path: "/my-relationships", icon: HeartHandshake },
  { label: "My Settings", path: "/my-settings", icon: User },
];

interface BuildInfo {
  version: string;
  commit_sha: string;
  build_date: string;
  image_tag: string | null;
}

function NavEntry({ item }: { item: NavItem }) {
  return (
    <li>
      <NavLink
        to={item.path}
        end={item.path === "/"}
        className={({ isActive }) =>
          cn(
            "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors",
            isActive
              ? "bg-accent text-accent-foreground"
              : "text-muted-foreground hover:bg-accent/50 hover:text-accent-foreground"
          )
        }
      >
        <item.icon className="h-4 w-4 shrink-0" />
        {item.label}
      </NavLink>
    </li>
  );
}

export function Sidebar({ className }: { className?: string }) {
  const { identity } = useAuthStore();
  // Token-only login (no identity) = static gateway token = admin. Email login = check role.
  const isAdmin = identity ? identity.role === "system_admin" : true;

  const [build, setBuild] = useState<BuildInfo | null>(null);
  useEffect(() => {
    gateway
      .get<BuildInfo>("/api/version")
      .then(setBuild)
      .catch(() => {}); // footer is decoration — never break the nav over it
  }, []);

  const sections = NAV_SECTIONS.map((section) => ({
    ...section,
    items: section.items.filter((item) => !item.adminOnly || isAdmin),
  })).filter((section) => section.items.length > 0);

  return (
    <aside className={cn("flex h-full w-64 flex-col border-r bg-card", className)}>
      <div className="flex h-14 items-center gap-2 border-b px-4">
        <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-primary text-primary-foreground font-bold text-sm">W</div>
        <span className="font-semibold text-lg">Workforce</span>
      </div>
      <nav className="flex-1 overflow-y-auto p-2" role="navigation">
        {sections.map((section, i) => (
          <div key={section.title ?? "top"}>
            {section.title ? (
              <p
                className={cn(
                  "px-3 pb-1.5 text-xs font-semibold uppercase tracking-wider text-foreground/80",
                  // Hairline divider above each titled group to separate sections;
                  // the first group sits under the header border so it doesn't need one.
                  i > 0 ? "mt-4 pt-4 border-t border-border/60" : "pt-3"
                )}
              >
                {section.title}
              </p>
            ) : (
              i > 0 && <div className="h-3" />
            )}
            <ul className="space-y-0.5">
              {section.items.map((item) => (
                <NavEntry key={item.path} item={item} />
              ))}
            </ul>
          </div>
        ))}
      </nav>
      <div className="border-t p-2 space-y-1">
        <ul className="space-y-0.5">
          {BOTTOM_ITEMS.map((item) => (
            <NavEntry key={item.path} item={item} />
          ))}
        </ul>
        {build && (
          <div
            className="px-3 pt-1 font-mono text-[10px] leading-4 text-muted-foreground/60"
            title={build.image_tag ?? build.commit_sha}
          >
            <p>{build.commit_sha === "dev" ? "dev" : `v${build.commit_sha.slice(0, 7)}`}</p>
            <p>{build.build_date}</p>
          </div>
        )}
      </div>
    </aside>
  );
}
