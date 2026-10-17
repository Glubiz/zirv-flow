import { useEffect, useRef } from "react";
import { useBlocker } from "react-router";

export interface LeaveGuard {
  /** The in-app address the person tried to open while changes were unsaved; null when none. */
  pending: string | null;
  stay: () => void;
  leave: () => void;
  /** Lets the next navigation through without asking, for one the page itself makes after a save. */
  release: () => void;
}

/**
 * While `dirty`, any navigation to another page (a link, Back or Forward, or code calling
 * `navigate`) is held by the router's blocker and reported as `pending` so the page can ask in its
 * own dialog, and closing or reloading the tab gets the browser's own warning.
 */
export function useLeaveGuard(dirty: boolean): LeaveGuard {
  const dirtyNow = useRef(dirty);
  const released = useRef(false);

  useEffect(() => {
    dirtyNow.current = dirty;
  });

  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      dirtyNow.current && !released.current && currentLocation.pathname !== nextLocation.pathname,
  );

  useEffect(() => {
    if (!dirty) return;
    function onUnload(event: BeforeUnloadEvent) {
      event.preventDefault();
    }
    window.addEventListener("beforeunload", onUnload);
    return () => window.removeEventListener("beforeunload", onUnload);
  }, [dirty]);

  return {
    pending: blocker.state === "blocked" ? blocker.location.pathname : null,
    stay: () => {
      if (blocker.state === "blocked") blocker.reset();
    },
    leave: () => {
      if (blocker.state === "blocked") blocker.proceed();
    },
    release: () => {
      released.current = true;
    },
  };
}
