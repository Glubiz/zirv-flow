import { useEffect, useRef } from "react";

/** Calls `reload` each time `signal` changes after the first render; the data on screen stays until it answers. */
export function useReloadOn(signal: number, reload: () => void): void {
  const seen = useRef(signal);
  useEffect(() => {
    if (seen.current === signal) return;
    seen.current = signal;
    reload();
  }, [signal, reload]);
}
