import { client, unwrap, useResource, type Resource } from "../../api";

export interface TemplateOption {
  id: string;
  name: string;
  category: string;
}

/** The workspace's live email templates that have a published version, for a send step to choose from. */
export function usePublishedTemplates(workspaceId: string): Resource<TemplateOption[]> {
  return useResource(async () => {
    const all = await unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId } } }));
    return all
      .filter((template) => template.channel === "email" && template.current_version !== null && template.current_version !== undefined)
      .map((template) => ({ id: template.id, name: template.name, category: template.category }));
  }, [workspaceId]);
}
