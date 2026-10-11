import { useWorkspaceAccess } from "../members";

export interface ListAccess {
  /** Create, rename, delete lists and change who is in them: any member, admin or the owner. */
  canWrite: boolean;
  /** See identifier values: workspace admins and the owner. Everyone else sees them hidden. */
  canSeeValues: boolean;
}

export function useListAccess(workspaceId: string): ListAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false, canSeeValues: false };
  }
  return {
    canWrite: access.data.canManage || access.data.role !== null,
    canSeeValues: access.data.canManage,
  };
}

export function sourceLabel(source: string): string {
  switch (source) {
    case "ui":
      return "Added in the app";
    case "api":
      return "Added through the API";
    case "import":
      return "CSV import";
    case "merge":
      return "Merged people";
    case "flow":
      return "A flow";
    default:
      return source;
  }
}

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}
