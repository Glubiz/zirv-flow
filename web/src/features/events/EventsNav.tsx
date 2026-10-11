import { NavLink } from "react-router";

export function EventsNav({ workspaceId }: { workspaceId: string }) {
  const base = `/w/${workspaceId}/events`;
  return (
    <nav aria-label="Events" className="subnav">
      <NavLink to={base} end>
        Schemas
      </NavLink>
      <NavLink to={`${base}/debugger`}>Debugger</NavLink>
      <NavLink to={`${base}/explorer`}>Explorer</NavLink>
      <NavLink to={`${base}/rejects`}>Rejects</NavLink>
      <NavLink to={`${base}/settings`}>Settings</NavLink>
    </nav>
  );
}
