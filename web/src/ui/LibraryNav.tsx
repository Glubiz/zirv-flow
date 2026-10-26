import { NavLink } from "react-router";

const TABS = [
  { path: "templates", label: "Templates" },
  { path: "themes", label: "Themes" },
  { path: "snippets", label: "Snippets" },
  { path: "images", label: "Images" },
] as const;

/** The tabs of the template library: what a message is built from. */
export function LibraryNav({ workspaceId }: { workspaceId: string }) {
  return (
    <nav className="library-nav" aria-label="Template library">
      {TABS.map((tab) => (
        <NavLink key={tab.path} to={`/w/${workspaceId}/${tab.path}`}>
          {tab.label}
        </NavLink>
      ))}
    </nav>
  );
}
