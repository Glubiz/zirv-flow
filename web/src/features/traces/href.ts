export function traceHref(workspaceId: string, traceId: string): string {
  return `/w/${workspaceId}/traces/${traceId}`;
}

/**
 * Where a subject id of a decision leads, or none when the app has no page for it yet.
 *
 * - `message` opens the message log of the person it went to, which needs the record's `person`
 *   subject; a record without one has nowhere to open it.
 * - `destination` has no page of its own yet, so it is shown as text.
 */
export function subjectHref(
  workspaceId: string,
  name: string,
  id: string,
  subjects: Record<string, unknown>,
): { href: string; text: string } | null {
  if (name === "message") {
    const person = subjects["person"];
    return typeof person === "string"
      ? { href: `/w/${workspaceId}/people/${person}?tab=messages`, text: "Messages of this person" }
      : null;
  }
  if (name === "person") {
    return { href: `/w/${workspaceId}/people/${id}`, text: "Person" };
  }
  if (name === "flow") {
    return { href: `/w/${workspaceId}/flows/${id}`, text: "Flow" };
  }
  return null;
}
