import { useWorkspaceAccess } from "../members";

interface ThemeAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create themes, save drafts and preview: any member. */
  canWrite: boolean;
  /** Publish, set the default and archive: workspace admins and the owner. */
  canAdmin: boolean;
  /** Publish: publisher and above. */
  canPublish: boolean;
}

export function useThemeAccess(workspaceId: string): ThemeAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false, canPublish: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false, canPublish: false };
  return {
    ready: true,
    canWrite: access.data.canEdit,
    canAdmin: access.data.canAdmin,
    canPublish: access.data.canPublish,
  };
}
