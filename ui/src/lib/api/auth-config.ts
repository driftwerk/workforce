import { gateway } from "../api-client";

export interface OidcAuthConfig {
  display_name: string;
  end_session_endpoint?: string | null;
  post_logout_redirect_uri: string;
}

export interface AuthConfig {
  oidc?: OidcAuthConfig;
}

export const authConfigApi = {
  get: () => gateway.get<AuthConfig>("/api/auth/config"),
};

// Lightweight synchronous cache so `auth-store.logout()` can build the
// RP-initiated logout URL without awaiting a network call. Populated by
// the login page and refreshed on subsequent fetches.
const AUTH_CONFIG_CACHE_KEY = "workforce-auth-config-cache";

export function cacheAuthConfig(cfg: AuthConfig): void {
  try {
    sessionStorage.setItem(AUTH_CONFIG_CACHE_KEY, JSON.stringify(cfg));
  } catch {
    /* sessionStorage unavailable — logout will fall back to local-only */
  }
}

export function getCachedAuthConfig(): AuthConfig | null {
  try {
    const raw = sessionStorage.getItem(AUTH_CONFIG_CACHE_KEY);
    if (!raw) return null;
    return JSON.parse(raw) as AuthConfig;
  } catch {
    return null;
  }
}
