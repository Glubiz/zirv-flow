import { useWorkspaceAccess } from "../members";

interface ThemeAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create themes, save drafts and preview: any member. */
  canWrite: boolean;
  /** Publish, set the default and archive: workspace admins and the owner. */
  canAdmin: boolean;
}

export function useThemeAccess(workspaceId: string): ThemeAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false };
  return {
    ready: true,
    canWrite: access.data.canManage || access.data.role !== null,
    canAdmin: access.data.canManage,
  };
}
