import { ApiError } from "./api-client";

const KIND_LABELS: Record<string, string> = {
  ios_device: "iPhone",
  desktop_device: "Desk",
  voice_device: "Voice device",
};

/** A person-readable name for a device kind; unknown kinds show as sent. */
export function deviceKindLabel(kind: string | null | undefined): string {
  if (!kind) return "iPhone";
  return KIND_LABELS[kind] ?? kind;
}

/** The server's own `{"error": "…"}` wording, when the body carries one. */
function serverWording(err: unknown): string | null {
  if (!(err instanceof Error)) return null;
  try {
    const parsed = JSON.parse(err.message);
    if (parsed && typeof parsed.error === "string" && parsed.error.trim()) {
      return parsed.error;
    }
  } catch {
    /* not JSON */
  }
  return null;
}

function statusOf(err: unknown): number | null {
  if (err instanceof ApiError) return err.status;
  if (err && typeof err === "object" && "status" in err) {
    const status = (err as { status: unknown }).status;
    if (typeof status === "number") return status;
  }
  return null;
}

/**
 * A plain sentence for a failed pairing call. The server's wording wins when
 * it sent one; otherwise the status decides.
 */
export function pairErrorMessage(err: unknown): string {
  const worded = serverWording(err);
  if (worded) return worded;
  switch (statusOf(err)) {
    case 404:
      return "This code is unknown or has expired. Ask your device for a new code.";
    case 409:
      return "This code has already been used.";
    case 403:
      return "You can't connect devices here. Ask an administrator to connect it for you.";
    case 429:
      return "Too many attempts. Please wait a few minutes and try again.";
    case 401:
      return "Your session has ended. Please sign in again.";
    default:
      return "Something went wrong. Please try again.";
  }
}

/** The account a device landed in, as one line: `Name (email)`. */
export function accountLine(identity: { display_name: string; email: string | null }): string {
  const name = identity.display_name || identity.email || "your account";
  return identity.email && identity.email !== name ? `${name} (${identity.email})` : name;
}
