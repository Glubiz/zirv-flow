import { useEffect, useRef } from "react";

export const POLL_MS = 5000;

/** Busy beats skipped in a row before a refresh goes ahead anyway and supersedes a request that never settles. */
export const MAX_SKIPPED_BEATS = 5;

/**
 * Calls `refresh` every few seconds while `active`. A beat is skipped while the tab is hidden
 * and while the previous refresh is still in flight (`busy`), so a refresh slower than the
 * interval is not invalidated by the next one and settles. After `MAX_SKIPPED_BEATS` busy beats in a
 * row the refresh goes ahead anyway, so a request that never settles (a half-open connection) is
 * superseded and polling does not stop for good. Hidden beats do not count toward that limit.
 */
export function usePolling(active: boolean, busy: boolean, refresh: () => void, intervalMs: number = POLL_MS): void {
  const latest = useRef({ busy, refresh });
  const skipped = useRef(0);
  useEffect(() => {
    latest.current = { busy, refresh };
    if (!busy) skipped.current = 0;
  });
  useEffect(() => {
    if (!active) return;
    skipped.current = 0;
    const timer = setInterval(() => {
      if (document.hidden) return;
      if (latest.current.busy && skipped.current < MAX_SKIPPED_BEATS) {
        skipped.current += 1;
        return;
      }
      skipped.current = 0;
      latest.current.refresh();
    }, intervalMs);
    return () => clearInterval(timer);
  }, [active, intervalMs]);
}
