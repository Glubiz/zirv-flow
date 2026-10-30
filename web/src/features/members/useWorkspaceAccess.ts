import { client, unwrap, useResource, type components, type Resource } from "../../api";
import { useSignedInUser } from "../session";

export type Member = components["schemas"]["MemberBody"];
export type MemberRole = "admin" | "publisher" | "editor" | "viewer";

export function useMembers(workspaceId: string | null): Resource<Member[]> {
  return useResource(
    () => workspaceId === null ? Promise.resolve([]) : unwrap(client.GET("/api/v1/workspaces/{ws}/members", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

export interface WorkspaceAccess {
  role: MemberRole | null;
  isOwner: boolean;
  /** Edit drafts and run tests: editor and above. */
  canEdit: boolean;
  /** Publish, pause, resume, archive, retry and start: publisher and above. */
  canPublish: boolean;
  /** Members, keys, credentials and settings: admin and the owner. */
  canAdmin: boolean;
  /** See personal values unmasked. */
  canViewPii: boolean;
}

export function accessFor(members: Member[], userId: string, isOwner: boolean): WorkspaceAccess {
  const mine = members.find((member) => member.user_id === userId);
  // An unknown role string is treated as the least privileged role, never as no membership.
  const role: MemberRole | null = mine
    ? mine.role === "admin" || mine.role === "publisher" || mine.role === "editor"
      ? mine.role
      : "viewer"
    : null;
  const canAdmin = isOwner || role === "admin";
  const canPublish = canAdmin || role === "publisher";
  const canEdit = canPublish || role === "editor";
  return { role, isOwner, canEdit, canPublish, canAdmin, canViewPii: canAdmin || mine?.view_pii === true };
}

export function useWorkspaceAccess(workspaceId: string | null): Resource<WorkspaceAccess> {
  const user = useSignedInUser();
  const members = useMembers(workspaceId);
  if (members.status === "ready") {
    return { ...members, data: accessFor(members.data, user.id, user.is_owner) };
  }
  return members;
}
