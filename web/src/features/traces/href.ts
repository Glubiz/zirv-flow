export function traceHref(workspaceId: string, traceId: string): string {
  return `/w/${workspaceId}/traces/${traceId}`;
}
