import { client, unwrap, useResource, type Resource } from "../../api";
import type { ThemeListItem } from "./model";

/** The workspace's themes by name, with the tokens and the template count a card shows. */
export function useThemes(workspaceId: string, archived = false): Resource<ThemeListItem[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/themes", { params: { path: { ws: workspaceId }, query: { archived } } })),
    [workspaceId, archived],
  );
}

/** What uses one theme; `reloadKey` changes to read it again. */
export function useThemeUsedBy(workspaceId: string, themeId: string, reloadKey = 0) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}/where-used", { params: { path: { ws: workspaceId, id: themeId } } })),
    [workspaceId, themeId, reloadKey],
  );
}

/** How many templates follow the default today, read fresh: the themes list, then the default's where-used. `null` when no theme is the default. */
export function useDefaultFollowers(workspaceId: string): Resource<number | null> {
  return useResource(async () => {
    const themes = await unwrap(client.GET("/api/v1/workspaces/{ws}/themes", { params: { path: { ws: workspaceId } } }));
    const current = themes.find((theme) => theme.is_default);
    if (!current) return null;
    const used = await unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}/where-used", { params: { path: { ws: workspaceId, id: current.id } } }));
    return used.default_templates.length;
  }, [workspaceId]);
}
