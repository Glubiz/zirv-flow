import { useWorkspaceAccess } from "../members";

interface SnippetAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create snippets, save drafts and preview: any member. */
  canWrite: boolean;
  /** Publish and archive: workspace admins and the owner. */
  canAdmin: boolean;
}

export function useSnippetAccess(workspaceId: string): SnippetAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false };
  return {
    ready: true,
    canWrite: access.data.canManage || access.data.role !== null,
    canAdmin: access.data.canManage,
  };
}
