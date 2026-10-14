import { useEffect, useState } from "react";
import { useNavigate } from "react-router";

export interface LeaveGuard {
  /** The in-app address the person tried to open while changes were unsaved; null when none. */
  pending: string | null;
  stay: () => void;
  leave: () => void;
}

/**
 * While `dirty`, a click on an in-app link is held back and reported as `pending` so the page can ask
 * in its own dialog, and closing or reloading the tab gets the browser's own warning. The router in use
 * is not a data router, so it has no blocker to ask; links are the way out of a page that this can see.
 */
export function useLeaveGuard(dirty: boolean): LeaveGuard {
  const navigate = useNavigate();
  const [pending, setPending] = useState<string | null>(null);

  useEffect(() => {
    if (!dirty) return;
    function onClick(event: MouseEvent) {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const link = event.target instanceof Element ? event.target.closest("a[href]") : null;
      const href = link?.getAttribute("href");
      if (!link || !href?.startsWith("/") || link.getAttribute("target") === "_blank") return;
      event.preventDefault();
      event.stopPropagation();
      setPending(href);
    }
    function onUnload(event: BeforeUnloadEvent) {
      event.preventDefault();
    }
    document.addEventListener("click", onClick, true);
    window.addEventListener("beforeunload", onUnload);
    return () => {
      document.removeEventListener("click", onClick, true);
      window.removeEventListener("beforeunload", onUnload);
    };
  }, [dirty]);

  return {
    pending,
    stay: () => setPending(null),
    leave: () => {
      const target = pending;
      setPending(null);
      if (target !== null) void navigate(target);
    },
  };
}
