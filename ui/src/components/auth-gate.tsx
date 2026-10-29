import { useEffect, type ReactNode } from "react";
import { useAuthStore, TOKEN_KEY, IDENTITY_KEY, OIDC_ID_TOKEN_KEY } from "@/stores/auth-store";
import { LoginPage } from "@/pages/login";
import { PageSkeleton } from "@/components/loading-skeleton";
import { takeReturnTo } from "@/lib/return-to";
import { router } from "@/router";

/**
 * Parse the URL hash for OIDC callback outcomes before the auth store
 * initialises. Three hash shapes are recognised:
 *
 *   #token=<jwt>&identity_id=<id>&display_name=<name>&role=<role>&id_token=<idt>
 *     → successful OIDC login; persist to sessionStorage, clear the hash.
 *   #error=<code>[&detail=<msg>]
 *     → OIDC or identity-resolution failure; stash in sessionStorage for
 *       the login page to surface as a toast.
 *   #logged_out=1
 *     → the IdP has completed RP-initiated logout and bounced back; clear
 *       any residual auth state and surface a "signed out" toast.
 *
 * Returns the page parked before an SSO sign-in (see `saveReturnTo`) when
 * a login just completed, so the caller can route back to it.
 */
function consumeOidcHash(): string | null {
  const hash = window.location.hash;
  if (!hash || hash.length < 2) return null;
  const params = new URLSearchParams(hash.slice(1));

  const token = params.get("token");
  if (token) {
    const identity = {
      id: params.get("identity_id") ?? "",
      display_name: params.get("display_name") ?? "",
      role: (params.get("role") ?? "member") as
        | "system_admin"
        | "manager"
        | "member",
    };
    sessionStorage.setItem(TOKEN_KEY, token);
    sessionStorage.setItem(IDENTITY_KEY, JSON.stringify(identity));
    const idToken = params.get("id_token");
    if (idToken) {
      sessionStorage.setItem(OIDC_ID_TOKEN_KEY, idToken);
    }
    history.replaceState(null, "", window.location.pathname);
    return takeReturnTo();
  }

  const err = params.get("error");
  if (err) {
    const detail = params.get("detail");
    sessionStorage.setItem(
      "workforce-oidc-error",
      detail ? `${err}: ${detail}` : err,
    );
    history.replaceState(null, "", window.location.pathname);
    return null;
  }

  if (params.get("logged_out") === "1") {
    sessionStorage.removeItem(TOKEN_KEY);
    sessionStorage.removeItem(IDENTITY_KEY);
    sessionStorage.removeItem(OIDC_ID_TOKEN_KEY);
    sessionStorage.setItem("workforce-oidc-logged-out", "1");
    history.replaceState(null, "", window.location.pathname);
  }
  return null;
}

export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, initialize } = useAuthStore();

  useEffect(() => {
    const returnTo = consumeOidcHash();
    initialize();
    // The router read its location before the hash was consumed; move it.
    if (returnTo) void router.navigate(returnTo, { replace: true });
  }, [initialize]);

  if (isLoading) return <PageSkeleton />;
  if (!isAuthenticated) return <LoginPage />;
  return <>{children}</>;
}
