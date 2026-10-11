import { useEffect, useRef } from "react";

export const POLL_MS = 5000;

/** Calls `refresh` every few seconds while `active`, and skips the beat while the tab is hidden. */
export function usePolling(active: boolean, refresh: () => void, intervalMs: number = POLL_MS): void {
  const latest = useRef(refresh);
  useEffect(() => {
    latest.current = refresh;
  });
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => {
      if (!document.hidden) latest.current();
    }, intervalMs);
    return () => clearInterval(timer);
  }, [active, intervalMs]);
}
