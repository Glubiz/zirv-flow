import { NavLink } from "react-router";

import { useWorkspaceAccess } from "../members";

/** The Destinations and Forwarding entries of the main menu: for workspace admins and the owner only. */
export function DestinationsNav({ workspaceId }: { workspaceId: string }) {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready" || !access.data.canAdmin) {
    return null;
  }
  return (
    <>
      <NavLink to={`/w/${workspaceId}/destinations`}>Destinations</NavLink>
      <NavLink to={`/w/${workspaceId}/forwarding`}>Forwarding</NavLink>
    </>
  );
}
