import { useEffect, useRef } from "react";

export const POLL_MS = 5000;

/**
 * Calls `refresh` every few seconds while `active`. A beat is skipped while the tab is hidden
 * and while the previous refresh is still in flight (`busy`), so a refresh slower than the
 * interval is not invalidated by the next one and settles.
 */
export function usePolling(active: boolean, busy: boolean, refresh: () => void, intervalMs: number = POLL_MS): void {
  const latest = useRef({ busy, refresh });
  useEffect(() => {
    latest.current = { busy, refresh };
  });
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => {
      if (!document.hidden && !latest.current.busy) latest.current.refresh();
    }, intervalMs);
    return () => clearInterval(timer);
  }, [active, intervalMs]);
}
