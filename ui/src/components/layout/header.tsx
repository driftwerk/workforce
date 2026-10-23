import { Breadcrumbs } from "@/components/breadcrumbs";
import { ThemeToggle } from "@/components/theme-toggle";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useCommandPaletteStore } from "@/stores/command-palette-store";
import { useAuthStore } from "@/stores/auth-store";
import { Search, Menu, LogOut } from "lucide-react";

interface HeaderProps { onMenuToggle?: () => void; }

export function Header({ onMenuToggle }: HeaderProps) {
  const openPalette = useCommandPaletteStore((s) => s.toggle);
  const identity = useAuthStore((s) => s.identity);
  const logout = useAuthStore((s) => s.logout);

  return (
    <header className="flex h-14 items-center justify-between border-b bg-card px-4 lg:px-6">
      <div className="flex items-center gap-3">
        {onMenuToggle && (
          <Button variant="ghost" size="icon" className="lg:hidden" onClick={onMenuToggle} aria-label="Toggle menu">
            <Menu className="h-5 w-5" />
          </Button>
        )}
        <Breadcrumbs />
      </div>
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="sm" className="hidden sm:flex gap-2 text-muted-foreground" onClick={openPalette}>
          <Search className="h-4 w-4" />
          <span className="text-xs">Search...</span>
          <kbd className="pointer-events-none hidden h-5 select-none items-center gap-1 rounded border bg-muted px-1.5 font-mono text-[10px] font-medium opacity-100 sm:flex">
            <span className="text-xs">⌘</span>K
          </kbd>
        </Button>
        <ThemeToggle />
        {identity && (
          <div className="hidden sm:flex items-center gap-2 pl-2 border-l ml-1">
            <span className="text-sm text-muted-foreground">{identity.display_name}</span>
            <Badge variant="outline" className="text-[10px] px-1 py-0">{identity.role.replace(/_/g, " ")}</Badge>
          </div>
        )}
        {/* Always visible: on mobile the sidebar is a drawer, so this is the only logout. */}
        <div className="flex items-center pl-2 border-l ml-1">
          <Button
            variant="ghost"
            size="sm"
            className="gap-2 text-muted-foreground"
            onClick={logout}
            aria-label="Log out"
          >
            <LogOut className="h-4 w-4" />
            <span className="hidden sm:inline text-xs">Logout</span>
          </Button>
        </div>
      </div>
    </header>
  );
}
