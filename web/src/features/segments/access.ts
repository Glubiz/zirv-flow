import { useWorkspaceAccess } from "../members";

export interface SegmentAccess {
  /** Create, save and refresh segments: any member, admin or the owner. */
  canWrite: boolean;
}

export function useSegmentAccess(workspaceId: string): SegmentAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false };
  }
  return { canWrite: access.data.canManage || access.data.role !== null };
}
