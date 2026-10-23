import { useAuthStore, TOKEN_KEY } from "@/stores/auth-store";

class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
    this.name = "ApiError";
  }
}

let refreshPromise: Promise<boolean> | null = null;

async function tryRefresh(): Promise<boolean> {
  if (refreshPromise) return refreshPromise;
  refreshPromise = (async () => {
    const token = useAuthStore.getState().token;
    if (!token) return false;
    try {
      const res = await fetch("/api/auth/refresh", {
        method: "POST",
        headers: { Authorization: `Bearer ${token}` },
      });
      if (!res.ok) return false;
      const data = await res.json();
      sessionStorage.setItem(TOKEN_KEY, data.token);
      useAuthStore.setState({ token: data.token });
      return true;
    } catch {
      return false;
    } finally {
      refreshPromise = null;
    }
  })();
  return refreshPromise;
}

async function send(
  baseUrl: string,
  path: string,
  options: RequestInit = {},
  retried = false,
): Promise<Response> {
  const token = useAuthStore.getState().token;
  // A FormData body needs the browser to set the multipart boundary;
  // injecting `Content-Type: application/json` here produces an
  // unparseable request. Detect and skip the default in that case.
  const isFormData =
    typeof FormData !== "undefined" && options.body instanceof FormData;
  const headers: Record<string, string> = {
    ...(isFormData ? {} : { "Content-Type": "application/json" }),
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
    ...((options.headers as Record<string, string>) ?? {}),
  };

  const res = await fetch(`${baseUrl}${path}`, { ...options, headers });

  if (res.status === 401 && !retried) {
    const refreshed = await tryRefresh();
    if (refreshed) {
      return send(baseUrl, path, options, true);
    }
    useAuthStore.getState().logout();
    throw new ApiError(401, "Unauthorized");
  }

  if (!res.ok) {
    const body = await res.text().catch(() => "Unknown error");
    throw new ApiError(res.status, body);
  }

  return res;
}

async function request<T>(baseUrl: string, path: string, options: RequestInit = {}): Promise<T> {
  const res = await send(baseUrl, path, options);

  if (res.status === 204) return undefined as T;

  const contentType = res.headers.get("content-type") ?? "";
  if (
    !contentType.includes("application/json") &&
    !contentType.includes("json")
  ) {
    return (await res.text()) as T;
  }
  return res.json();
}

/** A text body and the `ETag` it was served with. */
export interface Tagged {
  body: string;
  etag: string | null;
}

async function requestBlob(path: string, retried = false): Promise<Blob> {
  const token = useAuthStore.getState().token;
  const res = await fetch(path, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (res.status === 401 && !retried) {
    if (await tryRefresh()) return requestBlob(path, true);
    useAuthStore.getState().logout();
    throw new ApiError(401, "Unauthorized");
  }
  if (!res.ok) {
    throw new ApiError(res.status, await res.text().catch(() => "Unknown error"));
  }
  return res.blob();
}

export const gateway = {
  get: <T>(path: string) => request<T>("", path),
  getTagged: async (path: string): Promise<Tagged> => {
    const res = await send("", path);
    return { body: await res.text(), etag: res.headers.get("etag") };
  },
  getBlob: (path: string) => requestBlob(path),
  post: <T>(path: string, body?: unknown) =>
    request<T>("", path, {
      method: "POST",
      body: body ? JSON.stringify(body) : undefined,
    }),
  put: <T>(path: string, body?: unknown, headers: Record<string, string> = {}) =>
    request<T>("", path, {
      method: "PUT",
      body: body ? JSON.stringify(body) : undefined,
      headers,
    }),
  putText: <T>(path: string, text: string, headers: Record<string, string> = {}) =>
    request<T>("", path, {
      method: "PUT",
      body: text,
      headers: { "Content-Type": "text/plain", ...headers },
    }),
  postText: <T>(path: string, text: string) =>
    request<T>("", path, {
      method: "POST",
      body: text,
      headers: { "Content-Type": "text/plain" },
    }),
  postFormData: <T>(path: string, form: FormData) =>
    request<T>("", path, { method: "POST", body: form }),
  del: <T>(path: string) => request<T>("", path, { method: "DELETE" }),
};

// The admin router is mounted inside the gateway at `/api/admin` (with auth).
// Existing call sites still pass `/api/foo` paths — rewrite them transparently
// so we don't have to touch 30+ usage sites.
function toAdminPath(path: string): string {
  if (path.startsWith("/api/")) return `/api/admin/${path.slice(5)}`;
  return `/api/admin${path.startsWith("/") ? "" : "/"}${path}`;
}

export const admin = {
  get: <T>(path: string) => request<T>("", toAdminPath(path)),
  post: <T>(path: string, body?: unknown) =>
    request<T>("", toAdminPath(path), {
      method: "POST",
      body: body ? JSON.stringify(body) : undefined,
    }),
  put: <T>(path: string, body?: unknown) =>
    request<T>("", toAdminPath(path), {
      method: "PUT",
      body: body ? JSON.stringify(body) : undefined,
    }),
  del: <T>(path: string) => request<T>("", toAdminPath(path), { method: "DELETE" }),
};

export { ApiError };
