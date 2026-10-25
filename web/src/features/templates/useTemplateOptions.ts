import { client, unwrap, useResource, type Resource } from "../../api";

export interface TemplateOption {
  id: string;
  name: string;
  category: string;
}

/** The workspace's live templates of a channel that have a published version, for a step to choose from. */
export function usePublishedTemplates(workspaceId: string, channel: "email" | "webhook"): Resource<TemplateOption[]> {
  return useResource(async () => {
    const all = await unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId } } }));
    return all
      .filter((template) => template.channel === channel && template.current_version !== null && template.current_version !== undefined)
      .map((template) => ({ id: template.id, name: template.name, category: template.category }));
  }, [workspaceId, channel]);
}
