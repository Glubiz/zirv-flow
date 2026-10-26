import { client, unwrap, useResource, type Resource } from "../../api";
import type { SnippetListItem } from "./model";

/** The workspace's snippets by name; the archived ones too when asked. */
export function useSnippets(workspaceId: string, archived = false): Resource<SnippetListItem[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/snippets", { params: { path: { ws: workspaceId }, query: { archived } } })),
    [workspaceId, archived],
  );
}

export interface SnippetOption {
  id: string;
  name: string;
}

/** The live snippets that have a published version, for a snippet block or a theme footer to choose from. */
export function usePublishedSnippets(workspaceId: string): Resource<SnippetOption[]> {
  return useResource(async () => {
    const all = await unwrap(client.GET("/api/v1/workspaces/{ws}/snippets", { params: { path: { ws: workspaceId } } }));
    return all
      .filter((snippet) => snippet.current_version !== null && snippet.current_version !== undefined)
      .map((snippet) => ({ id: snippet.id, name: snippet.name }));
  }, [workspaceId]);
}

/** What uses one snippet; `reloadKey` changes to read it again. */
export function useSnippetUsedBy(workspaceId: string, snippetId: string, reloadKey = 0) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/snippets/{id}/where-used", { params: { path: { ws: workspaceId, id: snippetId } } })),
    [workspaceId, snippetId, reloadKey],
  );
}
