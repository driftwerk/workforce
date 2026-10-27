// Fetching a lazy chunk can fail transiently — a connection dropped mid-download,
// a flaky mobile network, or a single-instance server cycling during a deploy
// (`ERR_CONNECTION_CLOSED`). A bare `import()` that rejects bubbles to
// the ErrorBoundary and dead-ends the whole message until a manual hard reload.
// Re-attempting the fetch a few times, with a short backoff, recovers the common
// transient case without the user noticing.

export async function retryImport<T>(
  load: () => Promise<T>,
  attempts = 3,
  baseDelayMs = 300,
): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      return await load();
    } catch (error) {
      lastError = error;
      if (attempt < attempts - 1) {
        // Linear backoff (300ms, 600ms): enough for a cycling task to come back,
        // short enough that the skeleton doesn't linger if it's truly gone.
        await new Promise((resolve) => setTimeout(resolve, baseDelayMs * (attempt + 1)));
      }
    }
  }
  throw lastError;
}
