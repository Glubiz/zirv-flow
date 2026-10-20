import { useWorkspaceAccess } from "../members";

interface FlowAccess {
  /** Whether the answer is in; until then nobody is offered an edit. */
  ready: boolean;
  /** Create flows and change drafts: any member, admin or the owner, as the API lets a PATCH through. */
  canWrite: boolean;
}

export function useFlowAccess(workspaceId: string): FlowAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status === "loading") return { ready: false, canWrite: false };
  if (access.status === "error") return { ready: true, canWrite: false };
  return { ready: true, canWrite: access.data.canManage || access.data.role !== null };
}
