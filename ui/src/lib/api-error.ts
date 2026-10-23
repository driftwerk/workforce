/**
 * The gateway client throws an `Error` whose message is the raw `{"error":"…"}`
 * response body. Surface the inner message when there is one, so a refusal the
 * backend worded for a human reaches the human.
 */
export function apiErrorMessage(err: unknown): string {
  if (!(err instanceof Error)) return "Unknown error";
  try {
    const parsed = JSON.parse(err.message);
    if (parsed && typeof parsed.error === "string") return parsed.error;
  } catch {
    // not JSON — fall through to the raw message
  }
  return err.message;
}
