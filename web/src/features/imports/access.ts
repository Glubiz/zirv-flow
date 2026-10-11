import { useWorkspaceAccess } from "../members";

export interface ImportAccess {
  /** Upload, map, run and cancel: any member, admin or the owner. */
  canWrite: boolean;
  /** Not only the history but its figures are shown to everyone; sample values only to admins. */
  ready: boolean;
}

export function useImportAccess(workspaceId: string): ImportAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false, ready: false };
  }
  return { canWrite: access.data.canManage || access.data.role !== null, ready: true };
}
