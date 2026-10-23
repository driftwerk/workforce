import { Link, Outlet } from "react-router";
import { useAuthStore } from "@/stores/auth-store";
import { ErrorBoundary } from "@/components/error-boundary";
import "./games.css";

export default function GamesLayout() {
  const identity = useAuthStore((s) => s.identity);
  return (
    <div className="games-root">
      <header className="flex items-center justify-between px-5 py-4 sm:px-8">
        <Link to="/games" className="g-display text-2xl text-white">
          🎮 Game Night
        </Link>
        <div className="flex items-center gap-2">
          {identity && <span className="g-chip">👋 {identity.display_name}</span>}
          <Link to="/" className="g-chip">
            ← Workspace
          </Link>
        </div>
      </header>
      <main className="mx-auto w-full max-w-3xl px-4 pb-16 sm:px-6">
        <ErrorBoundary>
          <Outlet />
        </ErrorBoundary>
      </main>
    </div>
  );
}
