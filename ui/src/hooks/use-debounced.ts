import { useEffect, useState } from "react";

/** Returns `value` delayed by `delayMs`, resetting the timer on every change —
 * used to drive server queries from a fast-changing input without a request
 * per keystroke. */
export function useDebounced<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(id);
  }, [value, delayMs]);
  return debounced;
}
