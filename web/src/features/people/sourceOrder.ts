import { client, unwrap, useResource, type components } from "../../api";
import { shortId } from "../../ui";

type SourceBody = components["schemas"]["SourceBody"];

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const IMPORT_LABEL = /^[a-z0-9_-]{1,64}$/;

/** The entries that need no source of the workspace: what a record carries besides a source id. */
export const FIXED_ENTRIES: { value: string; label: string }[] = [
  { value: "api", label: "API" },
  { value: "native", label: "Contacts" },
];

/** How an entry of a source order reads; `sources` is null while the list is not known. */
export function entryLabel(entry: string, sources: SourceBody[] | null): string {
  const fixed = FIXED_ENTRIES.find((candidate) => candidate.value === entry);
  if (fixed) return fixed.label;
  if (entry.startsWith("flow:")) return `Flow ${shortId(entry.slice(5))}`;
  if (UUID.test(entry)) {
    const source = sources?.find((candidate) => candidate.id === entry);
    if (source) return source.name;
    return sources === null ? shortId(entry) : "Unknown source";
  }
  return IMPORT_LABEL.test(entry) ? `Import ${entry}` : "Unknown source";
}

/** A provenance source as the profile prints it: the source's name, or the raw value when the source is gone or unknown. */
export function sourceName(source: string, sources: SourceBody[] | null): string {
  return sources?.find((candidate) => candidate.id === source)?.name ?? source;
}

export function useWorkspaceSources(workspaceId: string) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sources", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}
