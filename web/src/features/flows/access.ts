import { useWorkspaceAccess } from "../members";

interface FlowAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create flows and change drafts: editor and above. */
  canWrite: boolean;
  /** Publish, pause, resume, archive, retry, skip and exit: publisher and above. */
  canPublish: boolean;
  /** Admin-only extras, such as the assistant. */
  canAdmin: boolean;
  /** Walk a real person in a test: needs Edit and the personal data grant. */
  canTestWithPerson: boolean;
}

export function useFlowAccess(workspaceId: string): FlowAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false, canPublish: false, canAdmin: false, canTestWithPerson: false };
  if (access.status === "error") return { ready: true, canWrite: false, canPublish: false, canAdmin: false, canTestWithPerson: false };
  return {
    ready: true,
    canWrite: access.data.canEdit,
    canPublish: access.data.canPublish,
    canAdmin: access.data.canAdmin,
    canTestWithPerson: access.data.canEdit && access.data.canViewPii,
  };
}
