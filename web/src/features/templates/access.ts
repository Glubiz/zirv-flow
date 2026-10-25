import { useWorkspaceAccess } from "../members";

interface TemplateAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create templates, save drafts and preview a sample person: any member. */
  canWrite: boolean;
  /** Publish and preview a real person: workspace admins and the owner. */
  canAdmin: boolean;
}

export function useTemplateAccess(workspaceId: string): TemplateAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false };
  return {
    ready: true,
    canWrite: access.data.canManage || access.data.role !== null,
    canAdmin: access.data.canManage,
  };
}
