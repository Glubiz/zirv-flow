import { useWorkspaceAccess } from "../members";

interface TemplateAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create templates, save drafts and preview a sample person: any member. */
  canWrite: boolean;
  /** Publish and preview a real person: workspace admins and the owner. */
  canAdmin: boolean;
  /** Publish: publisher and above. */
  canPublish: boolean;
  /** Preview or test with a real person: Edit plus the personal data grant. */
  canRealPerson: boolean;
}

export function useTemplateAccess(workspaceId: string): TemplateAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canAdmin: false, canPublish: false, canRealPerson: false };
  if (access.status === "error") return { ready: true, canWrite: false, canAdmin: false, canPublish: false, canRealPerson: false };
  return {
    ready: true,
    canWrite: access.data.canEdit,
    canAdmin: access.data.canAdmin,
    canPublish: access.data.canPublish,
    canRealPerson: access.data.canEdit && access.data.canViewPii,
  };
}
