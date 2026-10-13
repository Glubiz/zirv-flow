import { useWorkspaceAccess } from "../members";

export interface SourceAccess {
  /** Create, change, test, delete and set up streams: workspace admins and the owner only. */
  canManage: boolean;
  /** Whether the role has been looked up yet. */
  ready: boolean;
}

export function useSourceAccess(workspaceId: string): SourceAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canManage: false, ready: false };
  }
  return { canManage: access.data.canManage, ready: true };
}
