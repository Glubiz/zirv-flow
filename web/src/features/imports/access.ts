import { useWorkspaceAccess } from "../members";

export interface ImportAccess {
  /** Upload, map, dry-run and cancel: editor and above. */
  canWrite: boolean;
  /** Start the real run: publisher and above. */
  canPublish: boolean;
  ready: boolean;
}

export function useImportAccess(workspaceId: string): ImportAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false, canPublish: false, ready: false };
  }
  return { canWrite: access.data.canEdit, canPublish: access.data.canPublish, ready: true };
}
