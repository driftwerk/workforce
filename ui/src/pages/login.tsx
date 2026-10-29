import { useEffect, useState, type FormEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { useAuthStore } from "@/stores/auth-store";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Lock, Loader2, Key, LogIn } from "lucide-react";
import { gateway } from "@/lib/api-client";
import { authConfigApi, cacheAuthConfig } from "@/lib/api/auth-config";
import { saveReturnTo } from "@/lib/return-to";

export function LoginPage() {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [info, setInfo] = useState("");
  const [loading, setLoading] = useState(false);
  const [tokenMode, setTokenMode] = useState(false);
  const [token, setToken] = useState("");
  const [oidcLoading, setOidcLoading] = useState(false);
  const { login, loginWithToken } = useAuthStore();

  // Fetch the public auth config so we know whether to render the OIDC
  // button. This endpoint is unauthenticated and must not 401.
  const { data: authCfg } = useQuery({
    queryKey: ["auth-config"],
    queryFn: authConfigApi.get,
    staleTime: 5 * 60 * 1000,
  });
  useEffect(() => {
    if (authCfg) cacheAuthConfig(authCfg);
  }, [authCfg]);

  // Surface any OIDC callback outcomes parked in sessionStorage by
  // auth-gate.tsx (error from IdP, or "you've been signed out").
  useEffect(() => {
    const oidcError = sessionStorage.getItem("workforce-oidc-error");
    if (oidcError) {
      setError(`Sign-in failed: ${oidcError}`);
      sessionStorage.removeItem("workforce-oidc-error");
    }
    const loggedOut = sessionStorage.getItem("workforce-oidc-logged-out");
    if (loggedOut) {
      setInfo("You've been signed out.");
      sessionStorage.removeItem("workforce-oidc-logged-out");
    }
  }, []);

  const handleOidcLogin = async () => {
    setOidcLoading(true);
    setError("");
    // SSO comes back to `/`; park this page (e.g. `/pair?code=…`) to return to.
    saveReturnTo(window.location.pathname + window.location.search);
    try {
      const resp = await gateway.get<{ authorization_url: string }>(
        "/api/auth/oidc/login",
      );
      window.location.assign(resp.authorization_url);
    } catch {
      setError("Failed to start SSO login. Please try again or use email/password.");
      setOidcLoading(false);
    }
  };

  const handleEmailLogin = async (e: FormEvent) => {
    e.preventDefault();
    if (!email.trim() || !password.trim()) return;
    setLoading(true);
    setError("");
    try {
      const result = await login(email.trim(), password.trim());
      if (!result.ok) {
        setError(
          result.status === 429
            ? (result.message ??
                "Too many login attempts. Please wait a moment and try again.")
            : "Invalid email or password. If no users are configured, use the token login.",
        );
      }
    } finally {
      setLoading(false);
    }
  };

  const handleTokenLogin = async (e: FormEvent) => {
    e.preventDefault();
    if (!token.trim()) return;
    setLoading(true);
    setError("");
    try {
      const ok = await loginWithToken(token.trim());
      if (!ok) setError("Invalid token");
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-2 flex h-12 w-12 items-center justify-center rounded-full bg-primary/10">
            <Lock className="h-6 w-6 text-primary" />
          </div>
          <CardTitle>Workforce</CardTitle>
          <CardDescription>
            {tokenMode ? "Enter your gateway token" : "Sign in to continue"}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {tokenMode ? (
            <form onSubmit={handleTokenLogin} className="space-y-4">
              <div>
                <label htmlFor="login-token" className="sr-only">
                  Gateway token
                </label>
                <Input
                  id="login-token"
                  type="password"
                  placeholder="Gateway auth token"
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  autoFocus
                />
              </div>
              {error && (
                <p className="text-sm text-destructive" role="alert">
                  {error}
                </p>
              )}
              <Button
                type="submit"
                className="w-full"
                disabled={loading || !token.trim()}
              >
                {loading ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  "Authenticate"
                )}
              </Button>
              <button
                type="button"
                className="w-full text-sm text-muted-foreground hover:text-foreground"
                onClick={() => {
                  setTokenMode(false);
                  setError("");
                }}
              >
                Use email and password instead
              </button>
            </form>
          ) : (
            <form onSubmit={handleEmailLogin} className="space-y-4">
              <div>
                <label htmlFor="login-email" className="sr-only">
                  Email
                </label>
                <Input
                  id="login-email"
                  type="email"
                  placeholder="Email"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  autoFocus
                />
              </div>
              <div>
                <label htmlFor="login-password" className="sr-only">
                  Password
                </label>
                <Input
                  id="login-password"
                  type="password"
                  placeholder="Password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              </div>
              {info && (
                <p className="text-sm text-muted-foreground" role="status">
                  {info}
                </p>
              )}
              {error && (
                <p className="text-sm text-destructive" role="alert">
                  {error}
                </p>
              )}
              <Button
                type="submit"
                className="w-full"
                disabled={loading || !email.trim() || !password.trim()}
              >
                {loading ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  "Sign in"
                )}
              </Button>
              {authCfg?.oidc && (
                <>
                  <div className="flex items-center gap-2 py-1 text-xs text-muted-foreground">
                    <div className="h-px flex-1 bg-border" />
                    <span>or</span>
                    <div className="h-px flex-1 bg-border" />
                  </div>
                  <Button
                    type="button"
                    variant="outline"
                    className="w-full"
                    disabled={oidcLoading}
                    onClick={handleOidcLogin}
                  >
                    {oidcLoading ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : (
                      <>
                        <LogIn className="mr-2 h-4 w-4" />
                        Sign in with {authCfg.oidc.display_name}
                      </>
                    )}
                  </Button>
                </>
              )}
              <button
                type="button"
                className="flex w-full items-center justify-center gap-1.5 text-sm text-muted-foreground hover:text-foreground"
                onClick={() => {
                  setTokenMode(true);
                  setError("");
                }}
              >
                <Key className="h-3.5 w-3.5" />
                Use gateway token instead
              </button>
            </form>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
