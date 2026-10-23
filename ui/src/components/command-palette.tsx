import { useEffect } from "react";
import { useNavigate } from "react-router";
import { CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { useCommandPaletteStore } from "@/stores/command-palette-store";
import {
  LayoutDashboard, MessageSquare, Users, Bot, Network, GitBranch,
  Clock, Shield, Key, Settings, FileText, Workflow,
} from "lucide-react";

const PAGES = [
  { name: "Dashboard", path: "/", icon: LayoutDashboard },
  { name: "Chat", path: "/chat", icon: MessageSquare },
  { name: "Sessions", path: "/sessions", icon: FileText },
  { name: "Agents", path: "/agents", icon: Bot },
  { name: "Teams", path: "/teams", icon: Users },
  { name: "Workflows", path: "/workflows", icon: GitBranch },
  { name: "Pipelines", path: "/pipelines", icon: Workflow },
  { name: "Scheduler", path: "/scheduler", icon: Clock },
  { name: "Audit", path: "/audit", icon: Shield },
  { name: "Pairing", path: "/pairing", icon: Network },
  { name: "Vault", path: "/vault", icon: Key },
  { name: "Settings", path: "/settings", icon: Settings },
];

export function CommandPalette() {
  const { open, setOpen } = useCommandPaletteStore();
  const navigate = useNavigate();

  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen(!open);
      }
    };
    document.addEventListener("keydown", down);
    return () => document.removeEventListener("keydown", down);
  }, [open, setOpen]);

  const select = (path: string) => {
    setOpen(false);
    navigate(path);
  };

  return (
    <CommandDialog open={open} onOpenChange={setOpen}>
      <CommandInput placeholder="Type a command or search..." />
      <CommandList>
        <CommandEmpty>No results found.</CommandEmpty>
        <CommandGroup heading="Pages">
          {PAGES.map((page) => (
            <CommandItem key={page.path} onSelect={() => select(page.path)}>
              <page.icon className="mr-2 h-4 w-4" />
              <span>{page.name}</span>
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
