import { useWorkspaceAccess } from "../members";

export interface SegmentAccess {
  /** Create, save and refresh segments: any member, admin or the owner. */
  canWrite: boolean;
  /** Use the assistant on a segment: an admin or the owner. The assistant itself may still be off. */
  canUseAssistant: boolean;
}

export function useSegmentAccess(workspaceId: string): SegmentAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false, canUseAssistant: false };
  }
  return { canWrite: access.data.canManage || access.data.role !== null, canUseAssistant: access.data.canManage };
}
