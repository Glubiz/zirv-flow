import { client, unwrap, useResource, type components, type Resource } from "../../api";
import { useSignedInUser } from "../session";

export type Member = components["schemas"]["MemberBody"];
export type MemberRole = "admin" | "member";

export function useMembers(workspaceId: string): Resource<Member[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/members", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

export interface WorkspaceAccess {
  role: MemberRole | null;
  canManage: boolean;
}

export function accessFor(members: Member[], userId: string, isOwner: boolean): WorkspaceAccess {
  const mine = members.find((member) => member.user_id === userId)?.role;
  const role = mine === "admin" || mine === "member" ? mine : null;
  return { role, canManage: isOwner || role === "admin" };
}

export function useWorkspaceAccess(workspaceId: string): Resource<WorkspaceAccess> {
  const user = useSignedInUser();
  const members = useMembers(workspaceId);
  if (members.status === "ready") {
    return { ...members, data: accessFor(members.data, user.id, user.is_owner) };
  }
  return members;
}
