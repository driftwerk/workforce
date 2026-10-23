import { create } from "zustand";
import { getCachedAuthConfig } from "@/lib/api/auth-config";

// Audit UI-F1 (2026-05-15): auth state lives in sessionStorage rather
// than localStorage. Tab-scoped storage gives a smaller XSS blast
// radius — a stolen token can't outlive the tab — at the cost of
// re-login on browser restart. The robust fix is httpOnly cookies +
// CSRF, deferred until we can plumb cookie auth through the gateway.
export const TOKEN_KEY = "workforce-token";
export const IDENTITY_KEY = "workforce-identity";
export const OIDC_ID_TOKEN_KEY = "workforce-oidc-id-token";

export interface AuthIdentity {
  id: string;
  display_name: string;
  role: "system_admin" | "manager" | "member";
}

function parseStoredIdentity(raw: string | null): AuthIdentity | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed.id === "string" && typeof parsed.role === "string") {
      return parsed as AuthIdentity;
    }
  } catch { /* corrupt sessionStorage */ }
  return null;
}

/**
 * Outcome of an email/password login. Carries the HTTP status and the
 * server's error message so the login page can distinguish a rejected
 * credential (401) from a rate limit (429) and surface the right copy.
 */
export type LoginResult =
  | { ok: true }
  | { ok: false; status: number; message?: string };

interface AuthState {
  token: string | null;
  identity: AuthIdentity | null;
  isAuthenticated: boolean;
  isLoading: boolean;
  login: (email: string, password: string) => Promise<LoginResult>;
  loginWithToken: (token: string) => Promise<boolean>;
  logout: () => void;
  refresh: () => Promise<void>;
  initialize: () => void;
}

export const useAuthStore = create<AuthState>((set, get) => ({
  token: null,
  identity: null,
  isAuthenticated: false,
  isLoading: true,

  initialize: () => {
    const storedToken = sessionStorage.getItem(TOKEN_KEY);
    if (storedToken) {
      const identity = parseStoredIdentity(sessionStorage.getItem(IDENTITY_KEY));
      set({ token: storedToken, identity, isAuthenticated: true, isLoading: false });
      get().refresh();
    } else {
      set({ isLoading: false });
    }
  },

  login: async (email: string, password: string) => {
    try {
      const res = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ email, password }),
      });
      if (!res.ok) {
        const message = await res
          .json()
          .then((d) => (typeof d?.error === "string" ? d.error : undefined))
          .catch(() => undefined);
        return { ok: false, status: res.status, message };
      }
      const data = await res.json();
      const identity: AuthIdentity = {
        id: data.identity_id,
        display_name: data.display_name,
        role: data.role,
      };
      sessionStorage.setItem(TOKEN_KEY, data.token);
      sessionStorage.setItem(IDENTITY_KEY, JSON.stringify(identity));
      set({ token: data.token, identity, isAuthenticated: true });
      return { ok: true };
    } catch {
      return { ok: false, status: 0 };
    }
  },

  loginWithToken: async (token: string) => {
    try {
      const res = await fetch("/api/sessions", {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (!res.ok) return false;
      sessionStorage.setItem(TOKEN_KEY, token);
      sessionStorage.removeItem(IDENTITY_KEY);
      set({ token, identity: null, isAuthenticated: true });
      return true;
    } catch {
      return false;
    }
  },

  logout: () => {
    // If the session originated from OIDC and the IdP published an
    // end_session_endpoint, do a full RP-initiated logout so the Keycloak
    // session is terminated too. Otherwise just clear local state.
    const idToken = sessionStorage.getItem(OIDC_ID_TOKEN_KEY);
    const authCfg = getCachedAuthConfig();

    sessionStorage.removeItem(TOKEN_KEY);
    sessionStorage.removeItem(IDENTITY_KEY);
    sessionStorage.removeItem(OIDC_ID_TOKEN_KEY);
    set({ token: null, identity: null, isAuthenticated: false });

    if (idToken && authCfg?.oidc?.end_session_endpoint) {
      try {
        const u = new URL(authCfg.oidc.end_session_endpoint);
        u.searchParams.set("id_token_hint", idToken);
        const postLogout = new URL(authCfg.oidc.post_logout_redirect_uri);
        postLogout.hash = "logged_out=1";
        u.searchParams.set("post_logout_redirect_uri", postLogout.toString());
        window.location.assign(u.toString());
        return;
      } catch {
        /* fall through to local-only logout */
      }
    }
  },

  refresh: async () => {
    const { token } = get();
    if (!token) return;
    try {
      const res = await fetch("/api/auth/refresh", {
        method: "POST",
        headers: { Authorization: `Bearer ${token}` },
      });
      if (res.ok) {
        const data = await res.json();
        sessionStorage.setItem(TOKEN_KEY, data.token);
        set({ token: data.token });
      }
    } catch {
      // silent — token may still be valid
    }
  },
}));
