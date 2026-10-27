import type { ReactNode } from "react";
import { Navigate } from "react-router";

import { describeError } from "../../api";
import { ErrorState, LoadingState } from "../../ui";
import { useWorkspaceAccess } from "./useWorkspaceAccess";

/**
 * Shows its children to workspace admins and the owner only. Anyone else is sent to the members
 * page, so an admin-only route is not left as a dead end.
 */
export function RequireAdmin({ workspaceId, children }: { workspaceId: string; children: ReactNode }) {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") {
    return <LoadingState label="Checking your access" />;
  }
  if (access.status === "error") {
    return <ErrorState {...describeError(access.error)} onRetry={access.reload} />;
  }
  if (!access.data.canAdmin) {
    return <Navigate to={`/w/${workspaceId}/members`} replace />;
  }
  return <>{children}</>;
}
