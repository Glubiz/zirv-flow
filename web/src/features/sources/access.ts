import { useWorkspaceAccess } from "../members";

export interface SourceAccess {
  /** Create, change, test, delete and set up streams: workspace admins and the owner only. */
  canManage: boolean;
}

export function useSourceAccess(workspaceId: string): SourceAccess {
  const access = useWorkspaceAccess(workspaceId);
  return { canManage: access.status === "ready" && access.data.canManage };
}
