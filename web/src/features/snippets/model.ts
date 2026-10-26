import type { components } from "../../api";

export type Snippet = components["schemas"]["SnippetBody"];
export type SnippetDetail = components["schemas"]["SnippetDetailBody"];
export type SnippetUsedBy = components["schemas"]["SnippetUsedByBody"];
export type SnippetVersion = components["schemas"]["SnippetVersionBody"];
export type SnippetPreview = components["schemas"]["SnippetPreviewBody"];

/** The generated client types content as an object with no keys; this is the one place that says otherwise. */
export function asContent(content: object): Record<string, never> {
  return content as Record<string, never>;
}

export type UserKind = "template" | "theme" | "flow";

export const USER_LABEL: Record<UserKind, string> = { template: "Template", theme: "Theme", flow: "Active flow" };

export interface SnippetUser {
  kind: UserKind;
  id: string;
  name: string;
  href: string;
  /** Where in the template the snippet is held, when it is a template. */
  where?: string;
}

/** Everything that uses a snippet, templates first, then themes, then active flows. */
export function usersOf(workspaceId: string, used: SnippetUsedBy): SnippetUser[] {
  const base = `/w/${workspaceId}`;
  return [
    ...used.templates.map((template) => ({
      kind: "template" as const,
      id: template.id,
      name: template.name,
      href: `${base}/templates/${template.id}`,
      where: template.in_current ? (template.in_draft ? "draft and current version" : "current version") : "draft",
    })),
    ...used.themes.map((theme) => ({ kind: "theme" as const, id: theme.id, name: theme.name, href: `${base}/themes/${theme.id}` })),
    ...used.active_flows.map((flow) => ({ kind: "flow" as const, id: flow.flow_id, name: flow.flow_name, href: `${base}/flows/${flow.flow_id}` })),
  ];
}

/** The line under a snippet's name: always words. */
export function statusText(snippet: Snippet): string {
  if (snippet.archived_at) return "Archived";
  return snippet.current_version === null || snippet.current_version === undefined ? "Draft · not published" : `Draft · v${snippet.current_version} live`;
}

/** The version a publish would make next. */
export function nextVersion(snippet: Snippet): number {
  return (snippet.current_version ?? 0) + 1;
}
