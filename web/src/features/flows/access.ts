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
  /** Look people up by an identifier: the personal data grant. */
  canViewPii: boolean;
  /** Set when the lookup failed: every flag above is false then, which is safe but not the truth. */
  failure: { error: unknown; retry: () => void } | null;
}

const NONE = { canWrite: false, canPublish: false, canAdmin: false, canTestWithPerson: false, canViewPii: false };

export function useFlowAccess(workspaceId: string): FlowAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, ...NONE, failure: null };
  if (access.status === "error") return { ready: true, ...NONE, failure: { error: access.error, retry: access.reload } };
  return {
    ready: true,
    failure: null,
    canWrite: access.data.canEdit,
    canPublish: access.data.canPublish,
    canAdmin: access.data.canAdmin,
    canTestWithPerson: access.data.canEdit && access.data.canViewPii,
    canViewPii: access.data.canViewPii,
  };
}
