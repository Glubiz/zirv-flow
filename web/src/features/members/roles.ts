import type { MemberRole } from "./useWorkspaceAccess";

export const ROLE_OPTIONS = [
  { value: "admin", label: "Admin", help: "Access, credentials, settings" },
  { value: "publisher", label: "Publisher", help: "Publishes flows and templates" },
  { value: "editor", label: "Editor", help: "Edits drafts, runs tests" },
  { value: "viewer", label: "Viewer", help: "Reads only" },
];

export function roleLabel(role: string): string {
  return ROLE_OPTIONS.find((option) => option.value === role)?.label ?? role;
}

export function roleFrom(value: string): MemberRole {
  return value === "admin" || value === "publisher" || value === "viewer" ? value : "editor";
}
