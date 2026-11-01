import { useCallback } from "react";
import { useSearchParams } from "react-router";

/** The URL parameter that keeps the dock open, and the one that names its tab. */
const OPEN_PARAM = "assistant";
const TAB_PARAM = "dock";

interface AssistantDockState {
  open: boolean;
  setOpen: (open: boolean) => void;
  toggle: () => void;
}

/** Whether the assistant dock is open, kept in `?assistant=1` so a reload or a shared link shows it. */
export function useAssistantDock(): AssistantDockState {
  const [params, setParams] = useSearchParams();
  const open = params.get(OPEN_PARAM) === "1";
  const setOpen = useCallback(
    (next: boolean) => {
      setParams(
        (previous) => {
          const updated = new URLSearchParams(previous);
          if (next) {
            updated.set(OPEN_PARAM, "1");
          } else {
            updated.delete(OPEN_PARAM);
            updated.delete(TAB_PARAM);
          }
          return updated;
        },
        { replace: true },
      );
    },
    [setParams],
  );
  const toggle = useCallback(() => setOpen(!open), [open, setOpen]);
  return { open, setOpen, toggle };
}

/** Takes the dock's own parameters out of a set of search parameters, for a page that rebuilds its URL. */
export function withoutDock(params: URLSearchParams): URLSearchParams {
  const updated = new URLSearchParams(params);
  updated.delete(OPEN_PARAM);
  updated.delete(TAB_PARAM);
  return updated;
}
