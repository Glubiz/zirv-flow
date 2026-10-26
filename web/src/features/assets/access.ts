import { useWorkspaceAccess } from "../members";

interface AssetAccess {
  /** Whether the answer is in; until then nobody is offered an upload or a delete. */
  ready: boolean;
  /** Upload: any member. */
  canWrite: boolean;
  /** Delete: workspace admins and the owner. */
  canAdmin: boolean;
}

export function useAssetAccess(workspaceId: string): AssetAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false };
  return { ready: true, canWrite: access.data.canManage || access.data.role !== null, canAdmin: access.data.canManage };
}
