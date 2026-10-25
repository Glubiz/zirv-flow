import { NavLink } from "react-router";

import { useWorkspaceAccess } from "../members";

/** The Sending domains entry of the main menu: for workspace admins and the owner only. */
export function SendingNav({ workspaceId }: { workspaceId: string }) {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready" || !access.data.canManage) {
    return null;
  }
  return <NavLink to={`/w/${workspaceId}/sending-domains`}>Sending domains</NavLink>;
}
