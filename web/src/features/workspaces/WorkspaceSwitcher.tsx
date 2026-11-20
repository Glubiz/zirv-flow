import { useLocation, useNavigate } from "react-router";

import { MenuButton, type MenuEntry } from "../../ui";
import { useCurrentWorkspace, useWorkspaces } from "./WorkspacesProvider";

function Chevron() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
      <path d="M4 6l4 4 4-4" fill="none" stroke="currentColor" strokeWidth="1.6" />
    </svg>
  );
}

export function WorkspaceSwitcher({ compact = false }: { compact?: boolean }) {
  const list = useWorkspaces();
  const current = useCurrentWorkspace();
  const navigate = useNavigate();
  const location = useLocation();

  if (list.status === "loading") {
    return (
      <button type="button" className="pill-button" disabled>
        Loading workspaces
      </button>
    );
  }
  if (list.status === "error") {
    return (
      <button type="button" className="pill-button" onClick={list.reload}>
        Could not load workspaces. Try again
      </button>
    );
  }

  const section = /^\/w\/[^/]+\/([^/]+)/.exec(location.pathname)?.[1] ?? "";
  const entries: MenuEntry[] = list.data.map((workspace) => ({
    key: workspace.id,
    label: workspace.name,
    checked: workspace.id === current?.id,
    onSelect: () => void navigate(`/w/${workspace.id}${section ? `/${section}` : ""}`),
  }));
  entries.push({ key: "all", label: "All workspaces", onSelect: () => void navigate("/w") });

  // The first code point, not the first UTF-16 unit: an emoji must not be cut in half.
  const label = compact ? (Array.from(current?.name ?? "")[0] ?? "W") : (current?.name ?? "No workspace");

  return (
    <MenuButton
      label="Switch workspace"
      visibleText={label}
      placement={compact ? "beside" : "below-start"}
      buttonClassName="pill-button"
      buttonContent={
        <>
          <span className="dot" aria-hidden="true" />
          <span className="workspace-name">{label}</span>
          {compact ? null : <Chevron />}
        </>
      }
      entries={entries}
    />
  );
}
