import { NavLink } from "react-router";

import { useWorkspaceAccess } from "../members";

/** The Privacy entry of the main menu: shown to workspace admins and the owner only. */
export function PrivacyNav({ workspaceId }: { workspaceId: string }) {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready" || !access.data.canAdmin) {
    return null;
  }
  return <NavLink to={`/w/${workspaceId}/privacy`}>Privacy</NavLink>;
}
