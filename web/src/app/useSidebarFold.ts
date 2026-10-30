import { useState } from "react";

import { isFlowEditor } from "./navigation";

export const FOLD_STORAGE_KEY = "zirv-flow.sidebar-folded";

function remembered(): boolean {
  try {
    return globalThis.localStorage.getItem(FOLD_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

export function useSidebarFold(pathname: string) {
  const [preference, setPreference] = useState(remembered);
  const [override, setOverride] = useState<{ pathname: string; folded: boolean } | null>(null);
  if (override !== null && override.pathname !== pathname) setOverride(null);
  const folded = override?.pathname === pathname ? override.folded : isFlowEditor(pathname) || preference;

  function toggle() {
    const next = !folded;
    setOverride({ pathname, folded: next });
    // Unfolding the auto-folded editor is a one-page choice; it must not overwrite the stored preference.
    if (isFlowEditor(pathname)) return;
    setPreference(next);
    try {
      globalThis.localStorage.setItem(FOLD_STORAGE_KEY, String(next));
    } catch {
      // The menu remains usable when browser storage is unavailable.
    }
  }

  return { folded, toggle };
}
